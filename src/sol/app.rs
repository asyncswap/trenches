// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! The Solana dashboard — the pump.fun counterpart to the EVM trading screen.
//!
//! Deliberately mirrors the EVM layout field-for-field: same header (account /
//! slot / round-trip latency / sizing knobs), same Wallet + Market columns, and
//! the same direct panel keys (t/o/l/v) and arrow cycle. Renders through the
//! shared `view` models and `ui::widgets`, so both chains stay in step.
//!
//! Like the EVM side, **nothing auto-trades**: `b`/`s`/`x` are the only things
//! that ever place an order.

use std::collections::VecDeque;
use std::io::Stdout;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};
use ratatui::{prelude::*, widgets::*, Terminal};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

use super::discover::{self, SolSwap, TrenchRow};
use super::engine::{self, Coin};
use super::rpc::Rpc;
use super::{ata, lamports_to_sol, units_to_tokens};
use crate::ui;
use crate::view::{Cell, Col, PanelView, TableView, Tone};

type Term = Terminal<CrosstermBackend<Stdout>>;

const REFRESH: Duration = Duration::from_millis(1500);
const RING: usize = 200;
/// Signature window the tape asks for when a coin is quiet.
const TAPE_DEPTH: u32 = 40;
/// Ceiling for that window during a burst. `getSignaturesForAddress` costs the
/// same at any limit, and transactions are only fetched once, so a wide window
/// is cheap insurance against missing trades.
const TAPE_DEPTH_MAX: u32 = 250;

/// A coin loaded earlier this session, for the `p` picker.
///
/// Mirrors the EVM side's pool list: the point is to get back to something you
/// were already watching without re-pasting its mint.
#[derive(Debug, Clone)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SeenCoin {
    #[serde(with = "super::pubkey_b58")]
    pub mint: Pubkey,
    /// "SYMBOL · bonding curve" — the venue is part of the identity, since the
    /// same coin reads completely differently once it graduates.
    pub label: String,
}

/// Where the seen-coins list lives. Pasting a mint used to be a per-session
/// fact: close the app and the coin's name, and the address you hunted down,
/// were gone — repasted from scratch every time.
#[cfg(test)]
mod seen_coin_shape {
    use super::*;

    /// Round-trip + shape print, so a hand-written backfill file cannot
    /// silently fail load_coins' forgiving parse.
    #[test]
    fn seen_coins_round_trip() {
        let coins = vec![SeenCoin { mint: Pubkey::new_from_array([7u8; 32]), label: "X · curve".into() }];
        let text = serde_json::to_string(&coins).unwrap();
        println!("SHAPE: {text}");
        let back: Vec<SeenCoin> = serde_json::from_str(&text).unwrap();
        assert_eq!(back[0].mint, coins[0].mint);
    }

    /// The legacy byte-array shape — what an older build wrote — must load
    /// forever. This is the exact file that once came back empty and got
    /// overwritten by the next save.
    #[test]
    fn legacy_byte_array_mints_still_load() {
        let mint = Pubkey::new_from_array([9u8; 32]);
        let legacy = serde_json::json!({ "mint": mint.to_bytes().to_vec(), "label": "OLD · curve" });
        let coin = coin_of_value(legacy).expect("legacy entry must parse");
        assert_eq!(coin.mint, mint);
        assert_eq!(coin.label, "OLD · curve");
        // And a rotten row costs itself, not its neighbours.
        assert!(coin_of_value(serde_json::json!({ "mint": [1, 2, 3], "label": "?" })).is_none());
    }
}

/// The last coin the user was trading. A wallet switch rebuilds the whole
/// session, and an app restart obviously does — both used to come back with
/// no coin selected, the pool panel empty, and the user re-pasting what the
/// app knew perfectly well five seconds earlier. The EVM side has restored
/// its last pool since day one; this is the same courtesy.
fn last_coin_path() -> String {
    format!("{}/last-coin-solana.txt", crate::state_dir())
}

fn save_last_coin(mint: &Pubkey) {
    let _ = std::fs::write(last_coin_path(), mint.to_string());
}

fn load_last_coin() -> Option<Pubkey> {
    std::fs::read_to_string(last_coin_path()).ok()?.trim().parse().ok()
}

fn coins_path() -> String {
    format!("{}/coins-solana.json", crate::state_dir())
}

fn load_coins() -> Vec<SeenCoin> {
    let path = coins_path();
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    let Ok(vals) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else {
        // Not even a JSON list. Whatever it is, the one thing that must not
        // happen is the next save overwriting the only copy — that is exactly
        // how a format change once erased 18 coins. Move it aside instead.
        let _ = std::fs::rename(&path, format!("{path}.bad"));
        super::trace("coins file unreadable; set aside as coins-solana.json.bad");
        return Vec::new();
    };
    let total = vals.len();
    let coins: Vec<SeenCoin> = vals.into_iter().filter_map(coin_of_value).collect();
    if coins.len() < total {
        super::trace(&format!("coins file: salvaged {} of {total} entries", coins.len()));
    }
    coins
}

/// One seen-coin entry, from EITHER shape this file has ever had: the mint as
/// a base58 string (current) or as a 32-number byte array (what an older
/// build wrote). The array shape is what wiped the list once — the stricter
/// parser refused the whole file, load came back empty, and the next save
/// overwrote everything. Old shapes stay loadable forever.
fn coin_of_value(mut v: serde_json::Value) -> Option<SeenCoin> {
    if let Some(bytes) = pubkey_bytes(v.get("mint")) {
        v["mint"] = serde_json::Value::String(bs58::encode(&bytes).into_string());
    }
    serde_json::from_value(v).ok()
}

/// A JSON value that is a 32-entry byte array — the legacy pubkey encoding.
fn pubkey_bytes(v: Option<&serde_json::Value>) -> Option<Vec<u8>> {
    let arr = v?.as_array()?;
    let bytes: Vec<u8> = arr.iter().filter_map(|n| n.as_u64().map(|b| b as u8)).collect();
    (bytes.len() == 32).then_some(bytes)
}

fn save_coins(coins: &[SeenCoin]) {
    if let Ok(text) = serde_json::to_string_pretty(coins) {
        let path = coins_path();
        let tmp = format!("{path}.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Where one coin's sealed 1-minute candles live. The tape ring holds the
/// newest 500 trades; everything older survives HERE, aggregated once —
/// which is what lets a 4h or 1d chart exist at all, and keeps the chart
/// from re-walking every trade on every frame.
fn candles_path(mint: &Pubkey) -> String {
    format!("{}/candles-{mint}.json", crate::state_dir())
}

/// The most sealed minutes kept per coin — a week. ~800KB of JSON at worst,
/// bounded so neither the file nor the fold ever grows without limit.
const HIST_MAX: usize = 10_080;

fn load_candles(mint: &Pubkey) -> Vec<crate::view::Candle> {
    std::fs::read_to_string(candles_path(mint))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Debounced, like the tape file — and derived data: a lost write is only
/// a re-aggregation away, so no salvage ceremony is needed here.
fn save_candles(mint: &Pubkey, hist: &[crate::view::Candle]) {
    use std::sync::Mutex;
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if last.is_some_and(|t| t.elapsed().as_secs() < 5) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    if let Ok(text) = serde_json::to_string(hist) {
        let path = candles_path(mint);
        let tmp = format!("{path}.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Where one coin's tape history lives. The trades you watched — and MADE —
/// reload into the Trades view the moment the coin is selected again; the
/// live feed then merges on top (the (signature, event) dedup absorbs the
/// overlap), so history and now are one list.
fn tape_path(mint: &Pubkey) -> String {
    format!("{}/tape-{mint}.json", crate::state_dir())
}

fn load_tape_history(mint: &Pubkey) -> Vec<discover::SolSwap> {
    let Ok(text) = std::fs::read_to_string(tape_path(mint)) else { return Vec::new() };
    let Ok(vals) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else { return Vec::new() };
    // Per-entry, tolerant of the legacy byte-array pubkey shape, same story
    // as load_coins: one unreadable row must not cost the rest of the tape.
    vals.into_iter()
        .filter_map(|mut v| {
            if let Some(bytes) = pubkey_bytes(v.get("user")) {
                v["user"] = serde_json::Value::String(bs58::encode(&bytes).into_string());
            }
            serde_json::from_value(v).ok()
        })
        .collect()
}

/// Debounced full rewrite — the ring is bounded at TAPE_RING rows, so the
/// file is small and a rewrite beats bookkeeping appends.
fn save_tape_history(mint: &Pubkey, rows: &[discover::SolSwap]) {
    use std::sync::Mutex;
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if last.is_some_and(|t| t.elapsed().as_secs() < 3) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    if let Ok(text) = serde_json::to_string(rows) {
        let path = tape_path(mint);
        let tmp = format!("{path}.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Rungs of the network's priority-fee estimate.
///
/// `Medium` is roughly "will land eventually", `High` is what competitive
/// traffic on the same accounts is paying, `VeryHigh` is for a launch everyone
/// is hitting at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityLevel {
    Medium,
    High,
    VeryHigh,
}

impl PriorityLevel {
    /// The key the RPC uses for this rung.
    pub fn key(self) -> &'static str {
        match self {
            PriorityLevel::Medium => "medium",
            PriorityLevel::High => "high",
            PriorityLevel::VeryHigh => "veryHigh",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PriorityLevel::Medium => "medium",
            PriorityLevel::High => "high",
            PriorityLevel::VeryHigh => "very high",
        }
    }

    /// Step between rungs, clamped at both ends rather than wrapping — wrapping
    /// from the top back to the cheapest rung mid-launch is the last thing
    /// anyone wants from a key they are holding down.
    pub fn step(self, up: bool) -> PriorityLevel {
        match (self, up) {
            (PriorityLevel::Medium, true) => PriorityLevel::High,
            (PriorityLevel::High, true) => PriorityLevel::VeryHigh,
            (PriorityLevel::VeryHigh, true) => PriorityLevel::VeryHigh,
            (PriorityLevel::VeryHigh, false) => PriorityLevel::High,
            (PriorityLevel::High, false) => PriorityLevel::Medium,
            (PriorityLevel::Medium, false) => PriorityLevel::Medium,
        }
    }
}

/// Everything the background poller produces. The UI thread only ever *reads*
/// this — it never awaits RPC — so the dashboard stays responsive no matter how
/// slow the endpoint is. Mirrors the EVM side's `market`/`pool_cell` pattern.
#[derive(Default, Clone)]
struct Snapshot {
    sol: f64,
    token_bal: f64,
    slot: u64,
    curve: Option<super::pumpfun::BondingCurve>,
    /// AMM reserves as (SOL, coin) — by meaning, never by pool slot.
    amm_res: Option<(f64, f64)>,
    tape: Vec<SolSwap>,
    /// Fetched priority price, micro-lamports per CU. `None` when auto is off
    /// or the fetch failed — in which case the current setting simply stands.
    priority_micro: Option<u64>,
    /// Which coin this snapshot describes, or `None` for a wallet-only round.
    ///
    /// The poller runs concurrently with the UI, so a round already in flight
    /// when the user switches coins lands AFTER the switch. Without this stamp
    /// the dashboard absorbed the previous coin's trades as the new coin's —
    /// showing another token's tape under ansem's header.
    mint: Option<Pubkey>,
    round_ms: f64,
    sol_usd: f64,
}

/// What the poller needs to know about the selected coin.
#[derive(Clone, Copy)]
struct PollTarget {
    mint: Pubkey,
    /// The account whose signatures ARE this coin's trade history: the bonding
    /// curve while it's on the curve, the AMM pool once graduated. Reading the
    /// curve for a graduated coin returns its frozen launch-day trades forever.
    bonding_curve: Pubkey,
    ata: Pubkey,
    /// Set for AMM coins: the pool's (base, quote) vaults, whose balances ARE
    /// the reserves. Curve coins carry their reserves inside the curve account.
    amm_vaults: Option<(Pubkey, Pubkey)>,
    /// The COIN's decimals. pump coins are 6, but a graduated coin can be any
    /// SPL mint — assuming 6 misprices token amounts by orders of magnitude.
    token_decimals: u8,
    /// Which estimate rung to fetch, or `None` when priority is set by hand.
    /// Carried on the target so the poller — which owns the RPC — can fetch it
    /// alongside everything else instead of the UI thread ever awaiting.
    priority_level: Option<&'static str>,
    /// True when SOL occupies the pool's base slot (the common orientation on
    /// mainnet). Decides which vault holds SOL and which way trades read.
    sol_is_base: bool,
    /// Circulating supply, so the tape's market cap matches the Market panel's.
    total_supply: f64,
}

/// Per-coin decode cache for the tape.
///
/// `getTransaction` dominates RPC spend — the tape window barely moves between
/// rounds, so re-reading all of it every 1.5s was ~95% waste against a metered
/// API. Confirmed transactions are immutable, so each one is fetched exactly
/// once and kept.
#[derive(Default)]
struct TapeCache {
    mint: Option<Pubkey>,
    rows: Vec<SolSwap>,
    /// Every signature already inspected, including ones that decoded to
    /// nothing — otherwise those get re-fetched forever.
    seen: std::collections::HashSet<String>,
    /// Unanswered-fetch attempts per signature. Bandwidth is metered: a
    /// transaction no endpoint serves gets a handful of retries, then counts
    /// as seen rather than being re-downloaded every round until it scrolls
    /// out of the window.
    tries: std::collections::HashMap<String, u8>,
    /// How many signatures to ask for next round.
    depth: u32,
}

/// Give an unanswered signature this many rounds before writing it off.
/// Covers commitment lag and a throttled batch; gives up on the truly gone.
const TAPE_FETCH_TRIES: u8 = 5;

impl TapeCache {
    /// Point the cache at a coin, dropping another coin's history.
    fn retarget(&mut self, mint: Option<Pubkey>) {
        if self.mint != mint {
            self.mint = mint;
            self.rows.clear();
            self.seen.clear();
            self.tries.clear();
            self.depth = TAPE_DEPTH;
            // Seed from the coin's saved history: those transactions are
            // already on disk, and against a metered RPC plan, re-downloading
            // a window of trades the app already has is pure waste.
            if let Some(m) = mint {
                for row in load_tape_history(&m) {
                    self.seen.insert(row.signature.clone());
                    self.rows.push(row);
                }
            }
        }
    }

    /// The window to request this round.
    fn depth(&self) -> u32 {
        self.depth.max(TAPE_DEPTH)
    }

    /// Grow the window when a burst saturates it, shrink when things calm down.
    ///
    /// If EVERY signature returned was new, the window was full and trades
    /// almost certainly fell off the end unseen — a fixed window silently lost
    /// them, because signatures that scroll past are never asked for again.
    fn retune(&mut self, scanned: usize) {
        let d = self.depth();
        if scanned as u32 >= d {
            self.depth = (d * 2).min(TAPE_DEPTH_MAX);
            super::trace(&format!("tape window saturated at {d}, widening to {}", self.depth));
        } else if scanned as u32 * 4 < d {
            // Well under capacity — ease back toward the resting window.
            self.depth = (d / 2).max(TAPE_DEPTH);
        }
    }

    fn absorb(&mut self, batch: discover::TapeBatch) {
        self.retune(batch.fresh_total);
        self.seen.extend(batch.scanned);
        // Unanswered signatures get a bounded number of retries, then count
        // as seen — re-asking forever is bandwidth the tape never gets back.
        for sig in batch.unanswered {
            let n = self.tries.entry(sig.clone()).or_insert(0);
            *n += 1;
            if *n >= TAPE_FETCH_TRIES {
                self.seen.insert(sig.clone());
                self.tries.remove(&sig);
            }
        }
        discover::merge_tape(&mut self.rows, batch.rows);
        // Bound `seen` alongside the rows it guards.
        if self.seen.len() > discover::TAPE_RING * 4 {
            self.seen = self.rows.iter().map(|r| r.signature.clone()).collect();
        }
    }
}

/// Background poll loop: the ONLY place that touches RPC on a timer.
///
/// Independent reads run concurrently (`join!`) instead of one after another,
/// and only the bonding curve is re-read for the selected coin — its token
/// program and fee recipient never change. Sequential reads on the UI thread
/// were costing ~18 round trips a tick, which is where the 10s stalls came from.
#[allow(clippy::too_many_arguments)] // a swap needs every one of these; bundling them into a struct would only move the list
async fn poller(
    rpc: Rpc,
    trader: Pubkey,
    ws_urls: Vec<String>,
    target: Arc<Mutex<Option<PollTarget>>>,
    out: Arc<Mutex<Snapshot>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    rows_tx: tokio::sync::mpsc::UnboundedSender<(Pubkey, String, Vec<discover::SolSwap>)>,
    mut rows_rx: tokio::sync::mpsc::UnboundedReceiver<(Pubkey, String, Vec<discover::SolSwap>)>,
) {
    let http = reqwest::Client::new();
    let mut sol_usd = 0.0f64;
    let mut tape_cache = TapeCache::default();
    // The live tape rides the channel handed in by the session: the WS
    // subscription writes to it, and so does the confirmation path — your
    // own fill is injected the moment a trade confirms, from its own
    // signature, whether or not the socket heard it.
    let ws_alive = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut ws_task: Option<(Pubkey, Arc<std::sync::atomic::AtomicBool>, tokio::task::JoinHandle<()>)> = None;
    let mut tick: u64 = 0;
    let unix_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    };
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let t0 = Instant::now();
        let tgt = *target.lock().unwrap();
        tape_cache.retarget(tgt.as_ref().map(|t| t.mint));

        // Retargeting kills the old subscription and starts the new one.
        let want = tgt.as_ref().map(|t| t.bonding_curve);
        if ws_task.as_ref().map(|(a, _, _)| *a) != want {
            if let Some((_, s, h)) = ws_task.take() {
                s.store(true, std::sync::atomic::Ordering::Relaxed);
                h.abort();
            }
            ws_alive.store(0, std::sync::atomic::Ordering::Relaxed);
            if let (Some(t), false) = (tgt.as_ref(), ws_urls.is_empty()) {
                let target = discover::TapeTarget {
                    account: t.bonding_curve,
                    mint: t.mint,
                    amm: t.amm_vaults.map(|_| discover::AmmTapeParams {
                        token_decimals: t.token_decimals,
                        sol_is_base: t.sol_is_base,
                        total_supply: t.total_supply,
                    }),
                };
                let s = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let h = tokio::spawn(discover::watch_tape_ha(
                    rpc.clone(),
                    ws_urls.clone(),
                    target,
                    trader,
                    rows_tx.clone(),
                    ws_alive.clone(),
                    s.clone(),
                ));
                ws_task = Some((t.bonding_curve, s, h));
            }
        }
        // Drain pushed trades. Rows are tagged by mint, so anything from a
        // coin we've already left is dropped instead of haunting the tape.
        while let Ok((mint, sig, rows)) = rows_rx.try_recv() {
            if tape_cache.mint == Some(mint) && tape_cache.seen.insert(sig) {
                discover::merge_tape(&mut tape_cache.rows, rows);
            }
        }
        tick += 1;
        let ws_live = unix_ms().saturating_sub(ws_alive.load(std::sync::atomic::Ordering::Relaxed)) < 45_000;
        let poll_tape = !ws_live || tick.is_multiple_of(20);

        // Fire the independent reads together. The priority estimate rides
        // along in the same round so auto mode costs no extra latency, and is
        // scoped to THIS coin's state account — the thing every competing
        // trade must also write, and therefore what contention is priced on.
        let want_priority = tgt.as_ref().and_then(|t| t.priority_level.map(|l| (t.bonding_curve, l)));
        let (slot, bal, priority_micro) = tokio::join!(
            rpc.slot(),
            rpc.balance(&trader),
            async {
                match want_priority {
                    Some((acct, level)) => rpc.priority_fee_micro(&[acct], level).await,
                    None => None,
                }
            },
        );
        let (curve, amm_res, token_bal, tape) = match tgt {
            Some(t) => match t.amm_vaults {
                // AMM: reserves are the pool vaults' balances.
                Some((base_ta, quote_ta)) => {
                    // Three balances in ONE request instead of three.
                    let want = [base_ta, quote_ta, t.ata];
                    let (bals, batch) = tokio::join!(rpc.token_balances(&want), async {
                        if poll_tape {
                            // Graduated: trades live on the pool, not the curve.
                            discover::amm_tape(
                                &rpc,
                                &t.bonding_curve,
                                tape_cache.depth(),
                                &trader,
                                t.token_decimals,
                                t.sol_is_base,
                                t.total_supply,
                                &tape_cache.seen,
                            )
                            .await
                        } else {
                            discover::TapeBatch {
                                rows: Vec::new(),
                                scanned: Vec::new(),
                                unanswered: Vec::new(),
                                fresh_total: 0,
                            }
                        }
                    });
                    let bals = bals.unwrap_or_default();
                    let g = |i: usize| bals.get(i).copied().flatten().unwrap_or(0);
                    // Vault 0 is the pool's base slot, vault 1 its quote slot —
                    // which of those holds SOL depends on the orientation.
                    let (sol_raw, tok_raw) =
                        if t.sol_is_base { (g(0), g(1)) } else { (g(1), g(0)) };
                    let res = (
                        lamports_to_sol(sol_raw),
                        tok_raw as f64 / 10f64.powi(t.token_decimals as i32),
                    );
                    tape_cache.absorb(batch);
                    (None, Some(res), units_to_tokens(g(2)), tape_cache.rows.clone())
                }
                // Bonding curve: one account carries price and reserves.
                None => {
                    let (curve_acc, tb, batch) = tokio::join!(
                        rpc.account(&t.bonding_curve),
                        rpc.token_balance(&t.ata),
                        async {
                            if poll_tape {
                                discover::pool_tape(&rpc, &t.mint, tape_cache.depth(), &trader, &tape_cache.seen)
                                    .await
                            } else {
                                discover::TapeBatch {
                                    rows: Vec::new(),
                                    scanned: Vec::new(),
                                    unanswered: Vec::new(),
                                    fresh_total: 0,
                                }
                            }
                        },
                    );
                    let curve = curve_acc
                        .ok()
                        .flatten()
                        .and_then(|(d, _)| super::pumpfun::BondingCurve::decode(&d).ok());
                    tape_cache.absorb(batch);
                    (curve, None, units_to_tokens(tb.unwrap_or(0)), tape_cache.rows.clone())
                }
            },
            None => (None, None, 0.0, Vec::new()),
        };
        if sol_usd <= 0.0 {
            if let Some(p) = discover::sol_usd(&http).await {
                sol_usd = p;
            }
        }

        if let Ok(mut snap) = out.lock() {
            snap.slot = slot.unwrap_or(snap.slot);
            snap.sol = bal.map(lamports_to_sol).unwrap_or(snap.sol);
            if curve.is_some() {
                snap.curve = curve;
            }
            if amm_res.is_some() {
                snap.amm_res = amm_res;
            }
            snap.token_bal = token_bal;
            snap.priority_micro = priority_micro;
            // Publish the tape and the coin it belongs to TOGETHER. Keeping a
            // previous non-empty tape when a round came back empty let the old
            // coin's trades ride along under the new coin's stamp — the tape and
            // the identity have to come from the same round to mean anything.
            // The UI accumulates, so an empty window is harmless.
            snap.mint = tgt.as_ref().map(|t| t.mint);
            snap.tape = tape;
            snap.sol_usd = sol_usd;
            snap.round_ms = t0.elapsed().as_secs_f64() * 1000.0;
        }
        // The 30s RPC rollup. The EVM side writes it from its telemetry tick;
        // nothing on this path ever called it, so a whole Solana session went
        // by with the calls counted but the summary never written.
        crate::rpcstats::maybe_report();
        // The rest of the round is spent LISTENING, not sleeping: a trade the
        // websocket delivers goes on screen the moment it arrives, instead of
        // sitting in the channel until the next round drained it — which made
        // a live subscription feel like a 1.5s poll.
        let until = tokio::time::Instant::now() + REFRESH;
        loop {
            let now = tokio::time::Instant::now();
            if now >= until {
                break;
            }
            match tokio::time::timeout(until - now, rows_rx.recv()).await {
                Ok(Some((mint, sig, rows))) => {
                    let mut fresh = false;
                    if tape_cache.mint == Some(mint) && tape_cache.seen.insert(sig) {
                        discover::merge_tape(&mut tape_cache.rows, rows);
                        fresh = true;
                    }
                    while let Ok((mint, sig, rows)) = rows_rx.try_recv() {
                        if tape_cache.mint == Some(mint) && tape_cache.seen.insert(sig) {
                            discover::merge_tape(&mut tape_cache.rows, rows);
                            fresh = true;
                        }
                    }
                    if fresh {
                        if let Ok(mut snap) = out.lock() {
                            if snap.mint == tape_cache.mint {
                                snap.tape = tape_cache.rows.clone();
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }
}

/// The chart, straight off the tape: every swap IS a price at a time, so the
/// candles are a pure re-reading of data the dashboard already holds.
fn chart_view(bot: &SolBot) -> crate::view::CandleView {
    // OLDEST first: the tape displays newest-first, and block_time only has
    // second resolution — feeding the sorter newest-first flipped open and
    // close inside every same-second bucket, which painted the wrong colour.
    let points: Vec<(i64, f64, f64)> = bot
        .tape
        .iter()
        .rev()
        .filter(|s| !s.kind.is_lp() && s.tokens > 0.0)
        .filter_map(|s| s.block_time.map(|t| (t, s.sol / s.tokens, s.sol)))
        .collect();
    // The chain's clock, not the wall clock: block_time stamps the points, so
    // the flat line extends on the same axis. Falls back to the wall clock
    // only because the two agree within a second on Solana.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .ok();
    // Two sources, one grammar. Sub-minute charts read the live ring — their
    // whole window is minutes of the newest trades, which the ring always
    // covers. Minute-and-up charts fold the sealed 1m history, which reaches
    // back days where the ring holds barely an hour of a busy coin.
    let candles = if bot.chart_iv >= 60 && !bot.hist.is_empty() {
        let mut c = crate::view::fold_candles(&bot.hist, bot.chart_iv);
        if let Some(n) = now {
            crate::view::extend_flat_to_now(&mut c, bot.chart_iv, n, 240);
        }
        if c.len() > 240 {
            c.drain(..c.len() - 240);
        }
        c
    } else {
        crate::view::candles_of(&points, bot.chart_iv, 240, now)
    };
    let sym = bot
        .meta
        .as_ref()
        .map(|m| m.symbol.clone())
        .or_else(|| bot.coin.as_ref().map(|c| short_mint(&c.mint)))
        .unwrap_or_default();
    // Our own fills, straight off the tape's `mine` marks — the chart shows
    // where you got in and out, not just what the market did around you.
    let trades: Vec<(i64, f64, bool)> = bot
        .tape
        .iter()
        .filter(|s| s.mine && !s.kind.is_lp() && s.tokens > 0.0)
        .filter_map(|s| {
            s.block_time.map(|t| (t, s.sol / s.tokens, matches!(s.kind, discover::SwapKind::Buy)))
        })
        .collect();
    // Market cap is price times supply — a linear rescale, so the candles keep
    // their exact shape and only the axis changes. Same reasoning as the EVM
    // chart, and the same key.
    let supply = bot.coin.as_ref().map(|c| c.supply()).unwrap_or(0.0);
    let can_mcap = supply > 0.0;
    // Market cap is the number people compare launches in, and they compare it
    // in money — so the axis carries the rate through when we have one. Without
    // it the scale is supply alone and the axis stays SOL, which is true rather
    // than convenient. Same shape as the EVM chart, same reason.
    let mcap_money = bot.chart_mcap && can_mcap && bot.sol_usd > 0.0;
    let mcap_mul = if bot.chart_mcap && can_mcap {
        supply * if bot.sol_usd > 0.0 { bot.sol_usd } else { 1.0 }
    } else {
        1.0
    };
    let candles = if (mcap_mul - 1.0).abs() > f64::EPSILON {
        candles.into_iter().map(|c| c.scaled(mcap_mul)).collect()
    } else {
        candles
    };
    let trades: Vec<(i64, f64, bool)> =
        trades.into_iter().map(|(t, p, b)| (t, p * mcap_mul, b)).collect();
    crate::view::CandleView {
        // Coin and interval only — see the EVM chart for why. The axis says
        // which unit it is in without the title repeating it.
        title: format!(" {}/SOL · {} candle [,] [.] [m] ", sym, crate::view::iv_label(bot.chart_iv)),
        candles,
        interval_secs: bot.chart_iv,
        // Money carries its own symbol, so the unit label goes away with it.
        unit: if mcap_money { String::new() } else { "SOL".to_string() },
        money: mcap_money,
        // Solana swaps carry a real block time, so "now" is the wall clock —
        // no block-derived pseudo-clock to convert from.
        now_t: crate::ledger::now() as i64,
        active_key: None,
        trades,
    }
}

/// Lamports, and every token held: mint, raw amount, decimals, token program,
/// symbol.
type WalletAssets = (u64, Vec<(Pubkey, u64, u32, Pubkey, String)>);

/// How long a read of the wallet's tokens is worth trusting. Matches the EVM
/// dashboard's, and for the same reason.
const HELD_TTL: std::time::Duration = std::time::Duration::from_secs(120);

fn read_clock() -> &'static std::sync::Mutex<std::time::Instant> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> =
        std::sync::OnceLock::new();
    // Far enough back that the first open is never considered fresh.
    CELL.get_or_init(|| {
        std::sync::Mutex::new(std::time::Instant::now() - HELD_TTL - HELD_TTL)
    })
}

fn last_read() -> std::time::Instant {
    read_clock().lock().map(|g| *g).unwrap_or_else(|_| std::time::Instant::now())
}

fn mark_read() {
    if let Ok(mut g) = read_clock().lock() {
        *g = std::time::Instant::now();
    }
}

/// The last read of what this wallet holds.
///
/// Process-wide and deliberately not expiring on a timer: it is refreshed
/// every time the move screen opens, which is the only place that reads it, so
/// a clock would only ever agree with what the last open already decided.
fn wallet_assets() -> &'static std::sync::Mutex<Option<WalletAssets>> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<Option<WalletAssets>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(Default::default)
}

/// Read the wallet: balance, token accounts, and what each token is called.
///
/// The symbols resolve CONCURRENTLY. In series each one was a round trip the
/// next had to wait for, so the wait grew with the number of coins held —
/// which is backwards, since the wallets that most need this screen are the
/// ones holding the most.
async fn read_wallet_assets(rpc: &Rpc, me: &Pubkey) -> WalletAssets {
    use futures::StreamExt;
    let lamports = rpc.balance(me).await.unwrap_or(0);
    let held = rpc.owned_tokens(me).await.unwrap_or_default();
    let tokens: Vec<(Pubkey, u64, u32, Pubkey, String)> = futures::stream::iter(held)
        .map(|(mint, raw, dec, prog)| async move {
            let sym = if mint.to_string() == super::jupiter::USDC {
                "USDC".to_string()
            } else {
                // A mint with no on-chain name is still sendable; the address
                // is the identity and it is on the row either way.
                super::metadata::token_symbol(rpc, &mint)
                    .await
                    .unwrap_or_else(|| format!("{}…", &mint.to_string()[..6]))
            };
            (mint, raw, dec, prog, sym)
        })
        .buffered(8)
        .collect()
        .await;
    (lamports, tokens)
}

/// Ask what to move, where, how much — then confirm and send it.
///
/// Every prompt is a chance to stop, because the last one cannot be taken back.
/// The order is deliberate: the asset first (it decides the units), then the
/// destination (checked against the chain before an amount is ever typed), then
/// the amount, then the whole thing in one sentence to confirm.
async fn send_flow(term: &mut Term, bot: &mut SolBot) -> eyre::Result<()> {
    use super::send;
    let me = bot.signer.pubkey();
    let rpc = &bot.rpc;

    // What the wallet actually holds, read from the chain. USDC arrives by
    // transfer, not by a trade this app watched, so nothing it remembers would
    // ever list it.
    // What the wallet held last time this was opened, if it has been.
    //
    // Building this list costs a getProgramAccounts per token program plus a
    // metadata read per mint. Paying that before the picker can appear made
    // pressing M feel broken on a wallet holding more than a couple of things.
    // So: show what we know immediately, and refresh behind the screen for
    // next time.
    let cached = wallet_assets().lock().ok().and_then(|g| g.clone());
    let (lamports, tokens) = match cached {
        Some(hit) => hit,
        None => {
            bot.status = "reading balances…".into();
            let fresh = read_wallet_assets(rpc, &me).await;
            if let Ok(mut g) = wallet_assets().lock() {
                *g = Some(fresh.clone());
            }
            mark_read();
            fresh
        }
    };
    // Refresh for next time, off the drawing thread — but only when the last
    // read is old enough to have missed something. Re-reading on every press
    // spends a round trip per token to confirm what the previous press already
    // established, which on a wallet holding nothing is the whole cost of the
    // screen for an answer that has not changed.
    if last_read().elapsed() >= HELD_TTL {
        let (rpc2, me2) = (bot.rpc.clone(), me);
        tokio::spawn(async move {
            let fresh = read_wallet_assets(&rpc2, &me2).await;
            if let Ok(mut g) = wallet_assets().lock() {
                *g = Some(fresh);
            }
            mark_read();
        });
    }

    let mut choices: Vec<String> = vec![format!("SOL   {:.6}", lamports as f64 / 1e9)];
    for (mint, raw, dec, _prog, sym) in &tokens {
        choices.push(format!("{sym:<6} {:.6}   {mint}", *raw as f64 / 10f64.powi(*dec as i32)));
    }
    let Some(pick) = ui::select(term, "Move — what", &choices)? else { return Ok(()) };

    // The mint carries its OWN token program: classic and Token-2022 derive
    // different associated accounts, so the two travel together or the
    // transfer targets an account that does not exist.
    let (mint, decimals, held) = if pick == 0 {
        (None, 9u32, lamports)
    } else {
        let (m, raw, d, prog, _) = tokens[pick - 1];
        (Some((m, prog)), d, raw)
    };
    // The LIST may be a moment old; the amount must not be. One read, on the
    // one asset chosen — a cached balance that has since gone down would size
    // a transfer the chain then refuses, and "it said I had this" is a bad
    // thing for a wallet to have said.
    let held = match mint {
        None => rpc.balance(&me).await.unwrap_or(held),
        Some((m, prog)) => rpc
            .token_balance(&super::ata(&me, &m, &prog))
            .await
            .unwrap_or(held),
    };
    let symbol = choices[pick].split_whitespace().next().unwrap_or("?").to_string();

    // The destination, checked BEFORE an amount is typed — finding out the
    // address was wrong after entering the amount invites re-entering both.
    let Some(to_raw) = ui::input(term, "Move — to which address", "paste the recipient's address")?
    else {
        return Ok(());
    };
    let parsed: solana_pubkey::Pubkey = match to_raw.trim().parse() {
        Ok(k) => k,
        Err(_) => eyre::bail!("{}", send::Refusal::NotAnAddress.say()),
    };
    let executable = rpc.is_executable(&parsed).await;
    let mint_key = mint.map(|(m, _)| m);
    let to = send::check_destination(to_raw.trim(), &me, mint_key.as_ref(), executable)
        .map_err(|r| eyre::eyre!("{}", r.say()))?;

    // Does the recipient already have somewhere to put this token?
    let creates_account = match mint {
        None => false,
        Some((m, prog)) => {
            let dst = super::ata(&to, &m, &prog);
            rpc.account(&dst).await.ok().flatten().is_none()
        }
    };

    let cap = if mint.is_none() { send::max_sol_lamports(held) } else { held };
    let hint = format!("up to {:.6}", cap as f64 / 10f64.powi(decimals as i32));
    let Some(amount_raw) = ui::input(term, &format!("Move — how much {symbol}"), &hint)? else {
        return Ok(());
    };
    let whole: f64 = amount_raw.trim().parse().map_err(|_| eyre::eyre!("that is not a number"))?;
    if !(whole > 0.0) {
        eyre::bail!("nothing to send");
    }
    let amount = (whole * 10f64.powi(decimals as i32)).round() as u64;
    if amount > cap {
        // Named separately for SOL: "not enough" is confusing when the balance
        // clearly covers it, and the reason is the fee reserve.
        if mint.is_none() {
            eyre::bail!(
                "that would leave nothing for fees — {:.6} SOL is the most this can send",
                cap as f64 / 1e9
            );
        }
        eyre::bail!("you hold {:.6} {symbol}", held as f64 / 10f64.powi(decimals as i32));
    }

    let plan = send::Plan { to, mint: mint_key, symbol, amount, decimals, creates_account };
    if !ui::confirm(term, &plan.sentence())? {
        return Ok(());
    }

    let ixs = match mint {
        None => vec![send::transfer_sol(&me, &to, amount)],
        Some((m, prog)) => {
            send::transfer_token(&me, &to, &m, amount, decimals as u8, creates_account, &prog)
        }
    };
    bot.status = "sending…".into();
    let sig = super::tx::send(rpc, &bot.signer, ixs, 60_000, bot.cu_price_micro).await?;
    // On the orders list like anything else that left this wallet.
    //
    // A transfer is not a trade, but it IS money leaving on a keypress, and the
    // orders list is the record of exactly that. Leaving it out would mean the
    // one irreversible action in the app was the one action with no row.
    //
    // SOL amounts go in the SOL column and token amounts in the token column,
    // so a move reads in the same units as every row around it rather than
    // putting a token count where a SOL figure belongs.
    let (sol_moved, tokens_moved) = match plan.mint {
        None => (plan.ui_amount(), 0.0),
        Some(_) => (0.0, plan.ui_amount()),
    };
    // Pending, not confirmed: `send` submits, it does not confirm. The
    // settlement poll turns it into one or the other from the chain, the same
    // way every other order gets its answer.
    bot.push_order("SENT", sol_moved, tokens_moved, OrderState::Pending, Some(sig.clone()));
    crate::events::action(
        "Sent",
        &[("what", plan.sentence()), ("sig", sig.clone())],
    );
    bot.note(format!("Sent. {sig}"));
    Ok(())
}

/// Swap between the two assets here that are not memecoins: USDC and SOL.
///
/// Why this exists: you cannot buy anything on this chain with USDC. Arriving
/// with dollars and no SOL means the app can see the balance, name it, and do
/// nothing with it — and the fix, going back to an exchange to convert, is the
/// slowest possible answer to a problem the wallet is already holding.
///
/// Unlike every other trade in this app, the transaction is built by Jupiter
/// rather than here, because USDC/SOL liquidity is spread across Orca, Raydium
/// and Meteora and routing it is a different problem from swapping against one
/// known pool. `jupiter.rs` says what that costs and what narrows it. The part
/// that matters at this layer: the confirmation quotes the MINIMUM, not the
/// estimate, because the minimum is the only number the chain will enforce.
async fn swap_flow(term: &mut Term, bot: &mut SolBot) -> eyre::Result<()> {
    use super::{jupiter, send};
    let me = bot.signer.pubkey();
    let rpc = &bot.rpc;

    bot.status = "reading balances…".into();
    let lamports = rpc.balance(&me).await.unwrap_or(0);
    let usdc_mint: solana_pubkey::Pubkey =
        jupiter::USDC.parse().expect("the USDC mint is a checked constant");
    let usdc = rpc
        .owned_tokens(&me)
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|(m, ..)| *m == usdc_mint)
        .map(|(_, raw, ..)| raw)
        .unwrap_or(0);

    // The same reserve a transfer keeps back, for the same reason: swapping the
    // whole balance leaves nothing to pay for the swap.
    let sol_cap = send::max_sol_lamports(lamports);

    let ways = vec![
        format!("USDC → SOL     holding {:.2} USDC", usdc as f64 / 1e6),
        format!("SOL → USDC     holding {:.6} SOL", lamports as f64 / 1e9),
    ];
    let Some(way) = ui::select(term, "Swap — which way", &ways)? else { return Ok(()) };

    let (in_mint, out_mint, in_sym, out_sym, in_dec, out_dec, cap) = if way == 0 {
        (jupiter::USDC, jupiter::WSOL, "USDC", "SOL", 6u32, 9u32, usdc)
    } else {
        (jupiter::WSOL, jupiter::USDC, "SOL", "USDC", 9u32, 6u32, sol_cap)
    };
    if cap == 0 {
        eyre::bail!("no {in_sym} to swap");
    }

    let places = if in_dec == 6 { 2 } else { 6 };
    let hint = format!("up to {:.*}", places, cap as f64 / 10f64.powi(in_dec as i32));
    let Some(typed) = ui::input(term, &format!("Swap — how much {in_sym}"), &hint)? else {
        return Ok(());
    };
    let whole: f64 = typed.trim().parse().map_err(|_| eyre::eyre!("that is not a number"))?;
    if !(whole > 0.0) {
        eyre::bail!("nothing to swap");
    }
    let amount = (whole * 10f64.powi(in_dec as i32)).round() as u64;
    if amount > cap {
        // Named separately for SOL, as in the move flow: "not enough" reads as
        // wrong when the balance plainly covers it and the reserve is the
        // actual reason.
        if in_sym == "SOL" {
            eyre::bail!(
                "that would leave nothing for fees — {:.6} SOL is the most this can swap",
                cap as f64 / 1e9
            );
        }
        eyre::bail!("you hold {:.2} USDC", cap as f64 / 1e6);
    }

    // The slippage this dashboard already trades at, in the units Jupiter
    // wants. A second, separate slippage setting for one pair would be a
    // setting nobody remembers they have.
    let slippage_bps = (bot.slippage_pct * 100.0).round().clamp(1.0, 5_000.0) as u32;

    bot.status = "asking for a route…".into();
    let q = jupiter::quote(in_mint, out_mint, amount, slippage_bps).await?;
    let sent = jupiter::ui_amount(q.in_amount, in_dec);
    let expected = jupiter::ui_amount(q.out_amount, out_dec);
    let least = jupiter::ui_amount(q.min_out, out_dec);
    let out_places = if out_dec == 6 { 2 } else { 6 };

    // Both numbers, and which one is the promise. Showing only the estimate
    // would be quoting a figure that nothing enforces; showing only the floor
    // would look like a worse trade than the route expects.
    let question = format!(
        "Swap {sent:.*} {in_sym} for about {expected:.*} {out_sym}  (at least {least:.*} {out_sym}, or it reverts)",
        places, out_places, out_places
    );
    if !ui::confirm(term, &question)? {
        return Ok(());
    }

    bot.status = "swapping…".into();
    let sig = jupiter::execute(rpc, &bot.signer, &q).await?;

    // On the orders list, in the SOL column, because the SOL side is the part
    // of this trade the rest of the dashboard is denominated in. Pending, not
    // confirmed: `execute` submits, and the settlement poll gets the answer
    // from the chain like every other row.
    let sol_moved = if in_sym == "SOL" { sent } else { expected };
    bot.push_order("SWAP", sol_moved, 0.0, OrderState::Pending, Some(sig.clone()));
    crate::events::action("Swapped", &[("what", question), ("sig", sig.clone())]);
    bot.note(format!("Swapped. {sig}"));
    Ok(())
}

/// Which panel occupies the lower half — mirrors the EVM `Panel`.
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Orders,
    Tape,
    Logs,
    /// The candlestick chart, built from the same tape the Trades panel
    /// shows — no extra RPC, just a different way of reading it.
    Chart,
}

#[derive(Clone, Copy, PartialEq)]
pub enum OrderState {
    Pending,
    Confirmed,
    Failed,
}

pub struct SolOrder {
    /// When the order was sent, for the age column — the tape reads
    /// newest-first by age, and orders should read the same way.
    pub at: std::time::Instant,
    pub action: &'static str,
    pub sol: f64,
    /// Tokens this order moves. Set on sells (the amount sent in) so settlement
    /// can retire the matching SLICE of cost basis; 0 on buys, whose fill size
    /// is not known until it lands.
    pub tokens: f64,
    pub state: OrderState,
    pub sig: Option<String>,
    /// Market cap (SOL) at order time — the entry point, as on the EVM side.
    pub mc: f64,
    pub pooled: f64,
}

pub struct SolBot {
    /// Network label for the header, e.g. "Solana Mainnet".
    pub net: String,
    pub rpc: Rpc,
    pub signer: Keypair,
    pub coin: Option<Coin>,
    /// When graduation migration was last attempted, so a coin that has
    /// completed but whose pool isn't live yet is retried, not hammered.
    pub migrate_at: Option<Instant>,
    /// Name/symbol for the selected coin. Fetched on selection: coins added by
    /// mint have no launch event to read a name from.
    pub meta: Option<super::metadata::TokenMeta>,
    pub sol: f64,
    pub token_bal: f64,
    pub slot: u64,
    /// Round-trip time of the last refresh — the latency figure the EVM header shows.
    pub round_ms: f64,
    /// Buy size as a FRACTION of the SOL balance, like the EVM side's
    /// buy_frac — an absolute SOL amount was meaningless across wallet sizes.
    pub buy_frac: f64,
    /// The step `[` and `]` move by, when you have chosen one with `;` / `'`.
    /// None = derived from the wallet's size.
    pub buy_step_override: Option<f64>,
    pub slippage_pct: f64,
    /// Candle interval for the chart panel, seconds. , and . walk the ladder.
    /// Chart y axis: false = price, true = market cap. Mirrors the EVM `m`.
    pub chart_mcap: bool,
    pub chart_iv: u64,
    /// Fraction of the token balance `s` sells. `x` always sells everything.
    /// Mirrors the EVM side's `sell_frac`, adjusted with `<` / `>`.
    pub sell_frac: f64,
    pub cu_price_micro: u64,
    /// When set, `cu_price_micro` is refreshed from the network each poll
    /// instead of being dialled by hand.
    /// Coins loaded this session, most recent first.
    pub coins: Vec<SeenCoin>,
    pub priority_auto: bool,
    /// Which rung of the estimate to pay for. Contention changes minute to
    /// minute, so this is the knob that matters once auto is on.
    pub priority_level: PriorityLevel,
    // Cost basis / PnL, same model as the EVM engine.
    /// Whether a real keystore was unlocked. Everything that spends is guarded
    /// on this, so a throwaway signer can build the client without ever being
    /// able to send.
    pub has_account: bool,
    pub bought_qty: f64,
    pub bought_cost: f64,
    /// Unix seconds of the buy that opened the current position — the clock the
    /// hold time is measured from. Cleared on the sell that closes it.
    pub entry_at: Option<u64>,
    pub realized_pnl: f64,
    pub last_fill_pnl: Option<f64>,
    pub trades: u32,
    pub fails: u32,
    pub orders: VecDeque<SolOrder>,
    pub logs: VecDeque<String>,
    pub tape: Vec<SolSwap>,
    /// Sealed 1-minute candles for the selected coin — the tape ring's past,
    /// aggregated once and persisted. Big-interval charts fold THIS instead
    /// of re-walking trades, and it reaches back further than 500 rows ever
    /// could. Updated incrementally as the tape absorbs.
    pub hist: Vec<crate::view::Candle>,
    /// Unix seconds the selected coin launched, carried over from the trenches
    /// row so the Market panel can show its age like the EVM side does.
    pub launched_at: Option<i64>,
    /// Live SOL/USD, so caps can be shown in dollars. `0.0` = unknown, in which
    /// case the UI stays in SOL rather than inventing a rate.
    pub sol_usd: f64,
    /// Token risk scoring. Advisory only — never gates a trade.
    pub risk: super::rugcheck::RugCheck,
    pub warn_score: u32,
    http: reqwest::Client,
    pub status: String,
    /// Where confirmation-time fills are injected into the tape. Set once
    /// the session's channel exists.
    pub fills_tx: Option<tokio::sync::mpsc::UnboundedSender<(Pubkey, String, Vec<discover::SolSwap>)>>,
}

impl SolBot {
    /// What my own fills on THIS coin's tape say I hold: buys minus sells,
    /// newest evidence first. The tape hears a confirmed fill a second after
    /// it lands — long before the next balance poll — so this is the number
    /// that unblocks a fast exit. Never used to oversell: engine::sell clamps
    /// to the wallet's actual units at send time.
    pub fn tape_net_tokens(&self) -> f64 {
        let mut net = 0.0f64;
        for r in &self.tape {
            if !r.mine {
                continue;
            }
            match r.kind {
                super::discover::SwapKind::Buy => net += r.tokens,
                super::discover::SwapKind::Sell => net -= r.tokens,
                _ => {}
            }
        }
        net.max(0.0)
    }

}

/// The steps `;` and `'` move between, coarsest last. The EVM ladder, because
/// a key that adjusts the same thing should adjust it the same way.
const BUY_STEPS: [f64; 5] = [0.0001, 0.001, 0.005, 0.01, 0.05];

/// How much one press of `[` or `]` moves the buy size.
///
/// This used to walk a MULTIPLICATIVE ladder — 5% then 10, 25, 50, 100 — so
/// past the middle a single press could double the position. The EVM side has
/// always moved in even steps of 0.5%, and the same key doing something that
/// different depending on the chain is the thing this app keeps having to fix.
///
/// Finer on a large wallet, for the reason the EVM version gives: the step is a
/// fraction of the balance, so the bigger the balance the coarser the smallest
/// change you can make, which is backwards.
fn buy_step(bot: &SolBot) -> f64 {
    if let Some(s) = bot.buy_step_override {
        return s; // you said; that settles it
    }
    let wallet_usd = bot.sol * bot.sol_usd;
    if wallet_usd >= 1_000.0 { 0.001 } else { 0.005 }
}

/// Move the buy-size step one rung, and remember that you chose.
fn nudge_buy_step(bot: &mut SolBot, coarser: bool) {
    let now = buy_step(bot);
    let i = BUY_STEPS
        .iter()
        .enumerate()
        .min_by(|a, b| {
            (a.1 - now).abs().partial_cmp(&(b.1 - now).abs()).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(2);
    let next = if coarser { (i + 1).min(BUY_STEPS.len() - 1) } else { i.saturating_sub(1) };
    bot.buy_step_override = Some(BUY_STEPS[next]);
    // A finer step is pointless if the size cannot sit on it, and a coarser one
    // must not leave the size off its own grid.
    let step = BUY_STEPS[next];
    bot.buy_frac = ((bot.buy_frac / step).round() * step).clamp(step, 1.0);
    bot.note(format!("Buy step is now {}%", pct(step)));
}

/// A percentage label without noise: "0.1", "2.5", "5", "100".
fn pct(frac: f64) -> String {
    let p = frac * 100.0;
    if (p - p.round()).abs() < 1e-9 { format!("{}", p.round()) } else { format!("{p:.2}").trim_end_matches('0').trim_end_matches('.').to_string() }
}

/// The SOL a buy must leave behind, given what the next trade's priority fee
/// will cost: rent for the token account the buy creates, plus the fees for
/// TWO transactions — the buy, and the sell that has to be able to follow it.
/// A position you cannot exit is a worse outcome than an entry you never took.
///
/// Slack goes on the fee side only: the rent figure is exact, the priority
/// estimate is not and can be bumped between sizing and send.
///
/// Free function so it can be checked without standing up a bot.
fn reserve_for_buy(priority_sol: f64) -> f64 {
    let per_tx = super::lamports_to_sol(super::tx::SIGNATURE_FEE_LAMPORTS) + priority_sol;
    super::lamports_to_sol(super::tx::ATA_RENT_LAMPORTS) + per_tx * 2.0 * 1.25
}

impl SolBot {
    /// What a buy would spend right now: the fraction of the live balance, less
    /// the headroom a trade actually needs so fees never turn 100% into "not
    /// enough SOL".
    fn buy_size_sol(&self) -> f64 {
        (self.sol * self.buy_frac).min((self.sol - self.buy_reserve_sol()).max(0.0))
    }

    /// SOL a buy has to leave behind: the signature fee, the priority fee the
    /// NEXT trade will really pay, and rent for the token account a first buy
    /// creates — plus a little slack so a priority bump between sizing and send
    /// cannot make the transaction unaffordable.
    ///
    /// Derived rather than a round number. This was a flat 0.01 SOL, roughly
    /// four times the true cost, and because it lands in a `min` it behaves as
    /// a floor rather than a buffer: any wallet holding less than the reserve
    /// sized EVERY buy to exactly zero, whatever percentage was selected, and
    /// the only symptom was "buy size must be > 0".
    fn buy_reserve_sol(&self) -> f64 {
        reserve_for_buy(self.priority_fee_sol())
    }

    pub fn new(rpc: Rpc, signer: Keypair, rc: &crate::config::RugCheck, net: &str) -> SolBot {
        SolBot {
            net: net.to_string(),
            rpc,
            signer,
            coin: None,
            migrate_at: None,
            meta: None,
            sol: 0.0,
            token_bal: 0.0,
            slot: 0,
            round_ms: 0.0,
            buy_frac: 0.05, // 5% of balance; [ ] move it by `buy_step`
            buy_step_override: None,
            slippage_pct: 5.0,
            // The ONE canonical candle to perfect first: the minute. Other
            // intervals share every line of this code path, but the minute is
            // the reference the chart is judged against.
            chart_mcap: false,
            chart_iv: 60,
            sell_frac: 1.00,
            cu_price_micro: 10_000,
            coins: load_coins(),
            // ON by default: a launch-sniping buy without a priority fee lands
            // slots late, and by then the price has left the slippage floor
            // behind. [P] still toggles it off for quiet markets.
            priority_auto: true,
            priority_level: PriorityLevel::High,
            bought_qty: 0.0,
            has_account: true,
            bought_cost: 0.0,
            entry_at: None,
            realized_pnl: 0.0,
            last_fill_pnl: None,
            trades: 0,
            fails: 0,
            orders: VecDeque::new(),
            logs: VecDeque::new(),
            tape: Vec::new(),
            hist: Vec::new(),
            launched_at: None,
            sol_usd: 0.0,
            risk: super::rugcheck::RugCheck::new(rc),
            warn_score: rc.warn_score,
            http: reqwest::Client::new(),
            status: "Press f to find a coin".into(),
            fills_tx: None,
        }
    }

    pub fn trader(&self) -> Pubkey {
        self.signer.pubkey()
    }

    /// Average cost basis, SOL per token.
    pub fn avg_basis(&self) -> f64 {
        if self.bought_qty > 0.0 { self.bought_cost / self.bought_qty } else { 0.0 }
    }

    /// Slippage-aware value of selling everything now, minus what it cost.
    /// The holdings number worth trusting RIGHT NOW. The poll is authoritative
    /// at rest, but for ~a dozen seconds after one of our own fills it lags —
    /// a fresh buy reads as zero, a fresh exit reads as still held. When the
    /// tape heard one of our fills in the last 12s, the tape's net is the
    /// fresher witness, in both directions.
    pub fn effective_tokens(&self) -> f64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let fresh_own_fill = self
            .tape
            .iter()
            .filter(|r| r.mine)
            .filter_map(|r| r.block_time)
            .any(|bt| now - bt < 12);
        if fresh_own_fill {
            self.tape_net_tokens()
        } else {
            self.token_bal
        }
    }

    pub fn live_edge(&self) -> f64 {
        let held = self.effective_tokens();
        match &self.coin {
            Some(c) if held > 0.0 => c.sol_out(held) - self.bought_cost,
            _ => 0.0,
        }
    }

    fn note(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (h, m, s) = ((now % 86400) / 3600, (now % 3600) / 60, now % 60);
        let line = format!("[{h:02}:{m:02}:{s:02}] {msg}");
        super::trace(&format!("ui: {msg}"));
        // The ring holds 500 lines and dies with the process; the session log is
        // what is still there tomorrow when you want to know when you bought.
        super::session(&line);
        self.logs.push_back(line);
        while self.logs.len() > RING {
            self.logs.pop_front();
        }
        self.status = msg;
    }

    /// Log a background event: it belongs in the history, but must not seize
    /// the status line the way a user action does.
    fn event(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (h, m, s) = ((now % 86400) / 3600, (now % 3600) / 60, now % 60);
        super::trace(&format!("tape: {msg}"));
        let line = format!("[{h:02}:{m:02}:{s:02}] {msg}");
        super::session(&line);
        self.logs.push_back(line);
        while self.logs.len() > RING {
            self.logs.pop_front();
        }
    }

    /// Record a coin in the `p` picker, newest first, no duplicates.
    ///
    /// Called after a load SUCCEEDS — a mint that failed to load is not
    /// something to offer again from a menu.
    fn remember_coin(&mut self, coin: &Coin) {
        let sym = self
            .meta
            .as_ref()
            .map(|m| m.symbol.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| short_mint(&coin.mint));
        let label = format!("{sym}  ·  {}", coin.venue_label());
        self.coins.retain(|c| c.mint != coin.mint);
        self.coins.insert(0, SeenCoin { mint: coin.mint, label });
        self.coins.truncate(30);
        save_coins(&self.coins);
    }

    /// Hard ceiling on a single trade's priority fee, in SOL.
    ///
    /// The estimate's top rung (`unsafeMax`) reached 4.6e10 micro-lamports/CU
    /// while this was written — over 9 SOL for one trade. Auto mode must never
    /// be able to spend that by itself, whatever the endpoint returns.
    pub const PRIORITY_CAP_SOL: f64 = 0.001;

    /// What the current priority setting actually costs, in SOL.
    ///
    /// The fee is `micro_lamports x CU limit`, and the CU limit depends on the
    /// venue: an AMM swap touches ~23 accounts, a curve trade ~16. Rendering the
    /// same setting against different limits in two places showed two different
    /// numbers for one knob, neither tied to what the next trade would pay.
    fn priority_fee_sol(&self) -> f64 {
        super::tx::priority_fee_sol(self.cu_price_micro, self.cu_limit())
    }

    /// The compute limit the NEXT trade will carry.
    fn cu_limit(&self) -> u32 {
        match self.coin.as_ref().map(|c| c.on_amm()) {
            Some(false) => super::tx::CU_LIMIT_TRADE,
            _ => super::tx::CU_LIMIT_AMM,
        }
    }

    /// Accept a fetched estimate, clamped so one trade can never exceed
    /// [`Self::PRIORITY_CAP_SOL`] however wild the endpoint's answer is.
    fn apply_priority_estimate(&mut self, micro: u64) {
        let cu = self.cu_limit();
        let ceiling = (Self::PRIORITY_CAP_SOL * super::LAMPORTS_PER_SOL as f64) as u128
            * 1_000_000
            / cu.max(1) as u128;
        self.cu_price_micro = (micro as u128).min(ceiling) as u64;
    }

    /// True when the selected coin has filled its curve but is still being
    /// treated as a curve coin.
    fn needs_migration(&self) -> bool {
        matches!(
            self.coin.as_ref().map(|c| &c.venue),
            Some(engine::Venue::Curve { curve, .. }) if curve.complete
        )
    }

    /// Rate-limit migration attempts — the AMM pool appears a moment after the
    /// curve completes, so the first try often legitimately fails.
    fn migrate_due(&self) -> bool {
        self.migrate_at
            .map(|t| t.elapsed() >= Duration::from_secs(3))
            .unwrap_or(true)
    }

    fn push_order(
        &mut self,
        action: &'static str,
        sol: f64,
        tokens: f64,
        state: OrderState,
        sig: Option<String>,
    ) {
        let (mc, pooled) = self
            .coin
            .as_ref()
            .map(|c| (c.market_cap_sol(), c.pooled_sol()))
            .unwrap_or((0.0, 0.0));
        self.orders.push_back(SolOrder {
            at: std::time::Instant::now(),
            action,
            sol,
            tokens,
            state,
            sig,
            mc,
            pooled,
        });
        while self.orders.len() > RING {
            self.orders.pop_front();
        }
    }

    /// Copy the latest background snapshot into the bot. Pure memory work — no
    /// awaits — so it can run every frame without ever stalling the UI.
    /// Fold the tape's trades into sealed 1m candles and splice them over
    /// `hist`. The ring is authoritative for every minute it still covers —
    /// re-folding while trades are in the ring lets late arrivals correct a
    /// minute; once a minute scrolls out of the ring, its last fold stands.
    fn reseal_candles(&mut self) {
        let points: Vec<(i64, f64, f64)> = self
            .tape
            .iter()
            .rev()
            .filter(|s| !s.kind.is_lp() && s.tokens > 0.0)
            .filter_map(|s| s.block_time.map(|t| (t, s.sol / s.tokens, s.sol)))
            .collect();
        let fresh = crate::view::candles_of(&points, 60, HIST_MAX, None);
        let Some(first) = fresh.first() else { return };
        self.hist.retain(|c| c.t < first.t);
        // Stitch the seam: the fold had no context before the ring, so its
        // first candle opens at its own first trade — chain it to history.
        let mut fresh = fresh;
        if let Some(prev_close) = self.hist.last().map(|c| c.c) {
            if let Some(f) = fresh.first_mut() {
                // Open only — stretching h/l to the previous session's close
                // drew a chart-height wick line on the seam candle.
                f.o = prev_close;
            }
        }
        self.hist.extend(fresh);
        if self.hist.len() > HIST_MAX {
            let cut = self.hist.len() - HIST_MAX;
            self.hist.drain(..cut);
        }
    }

    /// Fill the Orders panel from the coin's saved history. The tape already
    /// remembers OUR fills (`mine`, the ⭐ rows) — the same record the chart's
    /// buy/sell lines and the PnL story draw from — so a coin traded last
    /// week arrives with its orders on the page, not an empty queue
    /// pretending nothing ever happened. Confirmed on arrival: `reap` only
    /// touches Pending orders, so nothing double-books.
    fn backfill_orders(&mut self) {
        let known: std::collections::HashSet<String> =
            self.orders.iter().filter_map(|o| o.sig.clone()).collect();
        // Oldest first, so the newest fill sits where the panel reads first.
        let fills: Vec<&SolSwap> = self
            .tape
            .iter()
            .rev()
            .filter(|s| s.mine && !s.kind.is_lp())
            .filter(|s| !known.contains(&s.signature))
            .collect();
        let mut seen = std::collections::HashSet::new();
        for s in fills {
            // One order per transaction — a sandwich of events is one press.
            if !seen.insert(s.signature.clone()) {
                continue;
            }
            // `Instant` cannot name a moment before the process started;
            // checked_sub keeps a fill from last week from panicking the age
            // column and falls back to "old" at the horizon.
            let at = s
                .block_time
                .and_then(|bt| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs() as i64;
                    std::time::Instant::now()
                        .checked_sub(std::time::Duration::from_secs((now - bt).max(0) as u64))
                })
                .unwrap_or_else(std::time::Instant::now);
            self.orders.push_back(SolOrder {
                at,
                action: if matches!(s.kind, discover::SwapKind::Buy) { "BUY" } else { "SELL" },
                sol: s.sol,
                tokens: s.tokens,
                state: OrderState::Confirmed,
                sig: Some(s.signature.clone()),
                mc: s.mkt_cap_sol,
                pooled: s.pooled_sol,
            });
        }
    }

    fn absorb(&mut self, snap: &Snapshot) {
        self.sol = snap.sol;
        self.slot = snap.slot;
        self.round_ms = snap.round_ms;
        self.sol_usd = snap.sol_usd;
        // Coin-specific data is only valid if the snapshot describes the coin
        // currently on screen. A stale round is dropped, not displayed.
        if snap.mint != self.coin.as_ref().map(|c| c.mint) {
            return;
        }
        // Balance of THIS coin's ATA — as stale-able as the tape.
        self.token_bal = snap.token_bal;
        // Only while auto is on: a round that was already in flight when it was
        // switched off must not overwrite a hand-set price.
        if self.priority_auto {
            if let Some(micro) = snap.priority_micro {
                self.apply_priority_estimate(micro);
            }
        }
        // Accumulate: each poll only returns the newest window.
        // The first fill is history, not news — logging it would bury the last
        // real events under a dozen lines the moment a coin is selected.
        let first_fill = self.tape.is_empty();
        let added = discover::merge_tape(&mut self.tape, snap.tape.clone());
        // New rows -> the coin's history file (debounced). This is what makes
        // a restart pick the story back up instead of starting it over.
        if !added.is_empty() {
            if let Some(m) = snap.mint {
                save_tape_history(&m, &self.tape);
                self.reseal_candles();
                save_candles(&m, &self.hist);
            }
        }
        if !first_fill {
            for t in added {
                let who = if t.mine { "  ⭐ you" } else { "" };
                self.event(match t.kind {
                    // LP moves are about the pool, so lead with the pool total.
                    discover::SwapKind::AddLp | discover::SwapKind::RemoveLp => format!(
                        "{} {:.4} SOL + {:.0} tok  pool now {:.2} SOL{who}",
                        t.kind.label(),
                        t.sol,
                        t.tokens,
                        t.pooled_sol
                    ),
                    _ => format!(
                        "{} {:.6} SOL  {:.0} tok  pooled {:.2} SOL{who}",
                        t.kind.label(),
                        t.sol,
                        t.tokens,
                        t.pooled_sol
                    ),
                });
            }
        }
        // Curve coins get their reserves from the snapshot; AMM coins are
        // refreshed in place by the poller via `refresh_venue`.
        match (self.coin.as_mut(), snap.curve.as_ref(), snap.amm_res) {
            (Some(coin), Some(curve), _) => {
                if let engine::Venue::Curve { curve: c, .. } = &mut coin.venue {
                    *c = curve.clone();
                }
            }
            (Some(coin), _, Some((b, q))) => {
                if let engine::Venue::Amm { sol_res, token_res, .. } = &mut coin.venue {
                    *sol_res = b;
                    *token_res = q;
                }
            }
            _ => {}
        }
    }

    /// Settle pending orders and book realized PnL, mirroring the EVM `reap`.
    async fn reap(&mut self) {
        let pending: Vec<(usize, String)> = self
            .orders
            .iter()
            .enumerate()
            .filter(|(_, o)| o.state == OrderState::Pending)
            .filter_map(|(i, o)| o.sig.clone().map(|s| (i, s)))
            .collect();
        // ONE getSignatureStatuses for every pending order — the method takes
        // an array; asking per order multiplied the poll by the queue depth.
        let sigs: Vec<String> = pending.iter().map(|(_, s)| s.clone()).collect();
        let Ok(statuses) = self.rpc.signatures_ok(&sigs).await else { return };
        for ((i, sig), status) in pending.into_iter().zip(statuses) {
            let Some(ok) = status else { continue };
            let (action, sol, sold_tok) = match self.orders.get(i) {
                Some(o) => (o.action, o.sol, o.tokens),
                None => continue,
            };
            if let Some(o) = self.orders.get_mut(i) {
                o.state = if ok { OrderState::Confirmed } else { OrderState::Failed };
            }
            // Neither a transfer nor a USDC/SOL swap is a trade in the sense
            // the rest of this loop means. Both settle like one — the signature
            // either landed or it did not — but neither buys or sells the coin
            // this dashboard is tracking, so everything below would be wrong
            // for them: it would move cost basis, count as a trade, and write a
            // fill to the ledger for a position that never changed.
            if matches!(action, "SENT" | "SWAP") {
                continue;
            }
            // Your own fill must never depend on the socket having heard it:
            // the signature is RIGHT HERE. Fetch that one transaction and
            // inject its rows into the tape — same decoders, same channel,
            // deduped by the seen-set like everything else. This is what
            // makes "buy, then sell immediately" reliable.
            if ok {
                if let (Some(coin), Some(fills)) = (&self.coin, &self.fills_tx) {
                    let tgt = poll_target(coin, &self.signer.pubkey(), None);
                    let rpc = self.rpc.clone();
                    let fills = fills.clone();
                    let sig2 = sig.clone();
                    let trader = self.signer.pubkey();
                    tokio::spawn(async move {
                        for wait_ms in [0u64, 300, 600, 1000, 1600] {
                            if wait_ms > 0 {
                                tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                            }
                            let Ok(tx) = rpc.transaction(&sig2).await else { continue };
                            if tx.is_null() {
                                continue;
                            }
                            let rows = if tgt.amm_vaults.is_some() {
                                discover::amm_swaps_in_tx(
                                    &tx,
                                    &sig2,
                                    &tgt.bonding_curve,
                                    &trader,
                                    tgt.token_decimals,
                                    tgt.sol_is_base,
                                    tgt.total_supply,
                                )
                            } else {
                                discover::curve_swaps_in_tx(&tx, &sig2, &tgt.mint, &trader)
                            };
                            let _ = fills.send((tgt.mint, sig2, rows));
                            return;
                        }
                    });
                }
            }
            // The WHOLE signature. A truncated one cannot be pasted into an
            // explorer, matched against a fill, or quoted in a bug report —
            // which is the entire reason it is written down.
            let short = sig.clone();
            if !ok {
                self.fails += 1;
                crate::events::error(
                    "Trade reverted",
                    &[("side", action.to_string()), ("sol", format!("{sol:.6}")), ("sig", short.clone())],
                );
                self.note(format!("The {action} for {sol:.6} SOL reverted, signature {short}"));
                continue;
            }
            self.trades += 1;
            // Cost basis: buys add, sells realise against what was paid.
            if action == "BUY" {
                self.bought_cost += sol;
                self.bought_qty = self.token_bal.max(self.bought_qty);
                // Adding to a position does not restart its clock.
                if self.entry_at.is_none() {
                    self.entry_at = Some(crate::ledger::now());
                }
                crate::events::trade(
                    "CONFIRMED BUY",
                    &[
                        ("sol", format!("{sol:.6}")),
                        ("coin", self.meta.as_ref().map(|m| m.symbol.clone()).unwrap_or_default()),
                        ("sig", short.clone()),
                    ],
                );
                self.note(format!("Buy for {sol:.6} SOL confirmed, signature {short}"));
            } else {
                // Retire only the slice of basis this sell actually consumed.
                // Zeroing the whole basis on a partial sell booked the entire
                // position's cost against a fraction of its proceeds — a fat
                // loss on a position still mostly open — and then handed the
                // remaining bag a zero basis, so its exit read as pure profit.
                // Mirrors the EVM `apply_fill`. A sell beyond the tracked
                // inventory is free-bag: zero cost, all profit.
                // Same function the EVM settle path uses — see the comment on
                // `realized_cost`. These were two copies and they had drifted.
                let (from_basis, cost) =
                    crate::engine::realized_cost(sold_tok, self.bought_qty, self.bought_cost);
                let pnl = sol - cost;
                self.realized_pnl += pnl;
                self.last_fill_pnl = Some(pnl);
                // The permanent record — `realized_pnl` is this session only.
                crate::ledger::append(
                    &self.signer.pubkey().to_string(),
                    &crate::ledger::Fill {
                        ts: crate::ledger::now(),
                        chain: self.net.clone(),
                        sym: self
                            .meta
                            .as_ref()
                            .map(|m| m.symbol.clone())
                            .unwrap_or_else(|| self.coin.as_ref().map(|c| short_mint(&c.mint)).unwrap_or_default()),
                        token: self.coin.as_ref().map(|c| c.mint.to_string()).unwrap_or_default(),
                        pnl,
                        cost,
                        proceeds: sol,
                        quote_sym: "SOL".into(),
                        quote_usd: self.sol_usd,
                        tx: sig.clone(),
                        held_secs: self.entry_at.map(|t| crate::ledger::now().saturating_sub(t)),
                        // NOT measured on this side yet. The EVM path reads gas
                        // off the receipt; the Solana fee is available the same
                        // way and is simply not wired up. Zero is the honest
                        // placeholder — it says "not counted", where a guessed
                        // 5000 lamports would say "counted" and be wrong.
                        gas: 0.0,
                        proof: String::new(), // filled by append, which reads the chain
                        pv: 0,                // likewise
                        verified: true,       // just made; nothing to distrust yet
                    },
                );
                self.bought_cost = (self.bought_cost - cost).max(0.0);
                self.bought_qty = (self.bought_qty - from_basis).max(0.0);
                // Only a closed position restarts the clock.
                if self.bought_qty <= 1e-12 {
                    self.entry_at = None;
                }
                crate::events::trade(
                    "CONFIRMED SELL",
                    &[
                        ("sol", format!("{sol:.6}")),
                        ("pnl", format!("{pnl:+.6} SOL")),
                        ("coin", self.meta.as_ref().map(|m| m.symbol.clone()).unwrap_or_default()),
                        ("sig", short.clone()),
                    ],
                );
                self.note(format!("Sell for {sol:.6} SOL confirmed with profit {pnl:+.6}, signature {short}"));
            }
        }
    }
}

/// The two accounts the poller watches for a coin — the state account (bonding
/// curve, or the AMM pool) and our token ATA. Venue-aware so graduated coins
/// poll their pool instead of a curve that no longer moves.
fn poll_target(coin: &Coin, trader: &Pubkey, priority_level: Option<&'static str>) -> PollTarget {
    match &coin.venue {
        engine::Venue::Curve { keys, .. } => PollTarget {
            mint: coin.mint,
            bonding_curve: keys.bonding_curve,
            ata: ata(trader, &keys.mint, &keys.token_program),
            amm_vaults: None,
            token_decimals: super::TOKEN_DECIMALS as u8,
            sol_is_base: false,
            priority_level,
            total_supply: coin.total_supply(),
        },
        engine::Venue::Amm { keys, token_decimals, sol_is_base, total_supply, .. } => PollTarget {
            mint: coin.mint,
            bonding_curve: keys.pool,
            ata: ata(trader, &keys.base_mint, &keys.base_token_program),
            amm_vaults: Some((keys.pool_base_ta, keys.pool_quote_ta)),
            token_decimals: *token_decimals,
            sol_is_base: *sol_is_base,
            priority_level,
            total_supply: *total_supply,
        },
    }
}

/// First 8 characters of a mint — enough to recognise a coin whose metadata
/// never loaded, without letting one row eat the menu width.
fn short_mint(mint: &Pubkey) -> String {
    mint.to_string().chars().take(8).collect()
}

// ---- view models ---------------------------------------------------------

/// Bold, fixed-width label in the accent/border colour — the same treatment as
/// the EVM columns, so panel frames, titles and parameter names read as one
/// family rather than three separate greys.
fn lbl(t: &str) -> Cell {
    Cell::bold(format!("{t:<11}"), Tone::Label)
}

fn wallet_panel(bot: &SolBot) -> PanelView {
    let mut p = PanelView::new(" Wallet [W] ");
    let pnl_tone = |v: f64| if v >= 0.0 { Tone::Good } else { Tone::Bad };

    // Nothing to report without an account.
    //
    // A column of zeroes is not "empty", it is a claim — zero balance, zero
    // realized, zero trades — and none of that is known until a key is unlocked.
    // Same shape as the Pool panel opposite: what is missing, then the key that
    // fixes it. The throwaway pubkey is never shown either; printing one invites
    // somebody to fund a key that exists only to build the client.
    if !bot.has_account {
        p.line("");
        p.line_toned("  No account", Tone::Normal);
        p.line("");
        p.line_toned("  [W] unlock or create an account", Tone::Info);
        return p;
    }

    // "Which account am I?" belongs with the balances, not in the header.
    p.spans(vec![
        lbl("Account"),
        Cell::bold(crate::account_line(&bot.trader().to_string()), Tone::Normal),
    ]);
    p.spans(vec![
        lbl("SOL"),
        Cell::new(if bot.sol_usd > 0.0 && bot.sol > 0.0 {
            format!("{:.6}  (${:.2})", bot.sol, bot.sol * bot.sol_usd)
        } else {
            format!("{:.6}", bot.sol)
        }),
    ]);
    p.spans(vec![lbl("Token"), Cell::new(format!("{:.4}", bot.token_bal))]);
    // Name the unit: "SOL/tok" is ambiguous once several coins are in play.
    let unit = bot.meta.as_ref().map(|m| m.symbol.as_str()).unwrap_or("tok");
    p.spans(vec![lbl("Basis"), Cell::new(format!("{:.9} SOL/{unit}", bot.avg_basis()))]);
    p.spans(vec![lbl("Inventory"), Cell::new(format!("{:.2} (bought)", bot.bought_qty))]);
    // Money leads, SOL rides beside it dimmed — the other way round from how
    // this started. A profit is a thing you reason about in the currency you
    // think in; six decimals of SOL is the raw figure, kept because it is what
    // the chain actually moved, not because it is what anyone reads first.
    //
    // With no rate, SOL is all there is, and it takes the front.
    let in_money = |v: f64| -> Vec<Cell> {
        if bot.sol_usd > 0.0 {
            vec![
                Cell::bold(crate::view::money_signed(v * bot.sol_usd), pnl_tone(v)),
                Cell::toned(format!("  {v:+.6} SOL"), Tone::Dim),
            ]
        } else {
            vec![Cell::bold(format!("{v:+.6} SOL"), pnl_tone(v))]
        }
    };
    let mut realized = vec![lbl("Realized")];
    realized.extend(in_money(bot.realized_pnl));
    p.spans(realized);
    let mut last_fill = vec![lbl("Last Fill")];
    match bot.last_fill_pnl {
        Some(v) => last_fill.extend(in_money(v)),
        None => last_fill.push(Cell::bold("—", Tone::Dim)),
    }
    p.spans(last_fill);
    let edge = bot.live_edge();
    let mut edge_row = vec![lbl("Edge")];
    edge_row.extend(in_money(edge));
    p.spans(edge_row);
    p.spans(vec![
        lbl("Activity"),
        Cell::new(format!(
            "{} trades  {} fails  {} pending",
            bot.trades,
            bot.fails,
            bot.orders.iter().filter(|o| o.state == OrderState::Pending).count()
        )),
    ]);
    p.spans(vec![
        lbl("Buy Size"),
        Cell::toned(format!("{}% ≈{:.4} SOL", pct(bot.buy_frac), bot.buy_size_sol()), Tone::Accent),
    ]);
    p.spans(vec![lbl("Slippage"), Cell::new(format!("{:.1}%", bot.slippage_pct))]);
    p.spans(vec![
        lbl("Priority"),
        Cell::new(format!(
            "{:.6} SOL",
            bot.priority_fee_sol()
        )),
    ]);
    p
}

fn market_panel(bot: &SolBot) -> PanelView {
    let mut p = PanelView::new(" Pool [p] ");
    match &bot.coin {
        None => {
            p.line("");
            p.line_toned("  No coin selected", Tone::Normal);
            p.line("");
            p.line_toned("  [f] find coins in the trenches", Tone::Info);
            p.line_toned("  [p] add a coin by mint address", Tone::Info);
        }
        Some(c) => {
            p.spans(vec![
                lbl("Venue"),
                Cell::bold(
                    if c.on_amm() { "PumpSwap AMM (graduated)" } else { "bonding curve" },
                    Tone::Normal,
                ),
            ]);
            if let Some(m) = &bot.meta {
                p.spans(vec![lbl("Token"), Cell::new(format!("{} ({})", m.name, m.symbol))]);
            }
            p.spans(vec![lbl("Mint"), Cell::new(c.mint.to_string())]);
            // Where to read more, one per row — two urls sharing a line
            // truncate each other on any normal width.
            //
            // High up, because a panel this tall runs out before it reaches the
            // bottom — these used to be six rows below Creator and were simply
            // never on screen. The coin's own page leads: every launch has one,
            // whether or not its creator filled in a single social.
            //
            // No "n/3 filled" count. It said nothing the links beside it do not,
            // and it cost the row they needed.
            {
                let mut links = vec![format!("https://pump.fun/coin/{}", c.mint)];
                if let Some(m) = &bot.meta {
                    links.extend(m.socials.iter().map(|(_, u)| u.clone()));
                    // The metadata file itself — the one thing that says whether
                    // a coin with no socials had none written or has a host that
                    // is down.
                    if let Some(url) = m.uri.as_deref().and_then(crate::net::metadata_url) {
                        links.push(url);
                    }
                }
                for (i, url) in links.iter().enumerate() {
                    let label = if i == 0 { lbl("Links") } else { lbl("") };
                    p.spans(vec![label, Cell::toned(url.clone(), Tone::Dim)]);
                }
            }
            // The pool is what you actually trade against, and it's the address
            // every chart and explorer keys off — worth showing next to the mint.
            if let Some(pair) = c.pair_address() {
                p.spans(vec![lbl("Pair"), Cell::new(pair.to_string())]);
            }
            let mint_s = c.mint.to_string();
            match bot.risk.cached(&mint_s) {
                Some(rep) => {
                    p.spans(vec![lbl("Risk"), Cell::bold(rep.summary(), rep.tone(bot.warn_score))]);
                    if rep.lp_locked_pct > 0.0 {
                        p.spans(vec![lbl("LP Locked"), Cell::new(format!("{:.0}%", rep.lp_locked_pct))]);
                    }
                }
                // Honest states instead of an eternal "checking…": a fetch in
                // the air says so; a recent failure says so; and a failure
                // whose retry window has passed RETRIES, right here — nothing
                // else re-asks for the selected coin.
                None if bot.risk.pending(&mint_s) => {
                    p.spans(vec![lbl("Risk"), Cell::toned("checking…", Tone::Dim)])
                }
                None if bot.risk.known(&mint_s) => {
                    p.spans(vec![lbl("Risk"), Cell::toned("unavailable — retrying soon", Tone::Dim)])
                }
                None => {
                    let (rk, m2) = (bot.risk.clone(), mint_s.clone());
                    tokio::spawn(async move {
                        let _ = rk.report(&m2).await;
                    });
                    p.spans(vec![lbl("Risk"), Cell::toned("checking…", Tone::Dim)])
                }
            }
            // In the currency the trader reads in, when we know what SOL is
            // worth. `usd_price` counts leading zeros rather than printing
            // them, which is the only way a 0.000000035 renders as anything
            // other than 0.00.
            p.spans(vec![
                lbl("Price"),
                Cell::new(if bot.sol_usd > 0.0 {
                    crate::view::usd_price(c.price_sol() * bot.sol_usd)
                } else {
                    // No rate: SOL is the honest unit, not a converted guess.
                    format!("{:.9} SOL", c.price_sol())
                }),
            ]);
            let mc = c.market_cap_sol();
            p.spans(vec![
                lbl("Mkt Cap"),
                Cell::new(if bot.sol_usd > 0.0 {
                    crate::view::usd_compact(mc * bot.sol_usd)
                } else {
                    format!("{mc:.0} SOL")
                }),
            ]);
            let pooled = c.pooled_sol();
            let pooled_usd = if bot.sol_usd > 0.0 {
                format!("  ({})", crate::view::usd_compact(pooled * bot.sol_usd))
            } else {
                String::new()
            };
            // Pooled is the REAL exit liquidity; market cap on a fresh coin is
            // nominal (the curve seeds 30 virtual SOL, so every launch reads
            // ~28 SOL cap with nothing actually pooled).
            p.spans(vec![
                lbl("Pooled"),
                Cell::toned(
                    format!("{} SOL{pooled_usd}", crate::view::sol_compact(pooled)),
                    if pooled < 0.5 { Tone::Warn } else { Tone::Normal },
                ),
            ]);
            p.spans(vec![lbl("Supply"), Cell::new(format!("{:.0} tokens", c.total_supply()))]);
            p.spans(vec![
                lbl("Bonded"),
                Cell::toned(
                    format!("{:.1}%", c.progress() * 100.0),
                    if c.progress() > 0.8 { Tone::Good } else { Tone::Normal },
                ),
            ]);
            let age = bot
                .launched_at
                .and_then(|bt| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs() as i64;
                    Some(crate::view::age_compact((now - bt).max(0) as f64))
                })
                .unwrap_or_else(|| "—".into());
            p.spans(vec![lbl("Age"), Cell::new(format!("{age} (since launch)"))]);
            // Shown raw, always. `1111…1111` (the all-zeros key) is meaningful
            // in itself: the pool carries no creator-fee recipient, which is
            // how a third-party-created pool differs from a pump migration.
            p.spans(vec![lbl("Creator"), Cell::new(c.creator().to_string())]);

        }
    }
    p
}

fn orders_table(bot: &SolBot, scroll: usize, h: usize) -> TableView {
    let total = bot.orders.len();
    let title = if total > h {
        format!(
            " My Orders {}–{} of {} ↑/↓ scroll  ·  {} ",
            scroll + 1,
            (scroll + h).min(total),
            total,
            crate::account_label(&bot.trader().to_string())
        )
    } else {
        // The trader is the same wallet on every row, so it belongs in the
        // title once rather than eating 44 columns per line.
        format!(" My Orders ({total}) ")
    };
    let mut t = TableView::new(
        title,
        vec![
            // Age leads, as on the tape: these are the same events seen from
            // our side, so they should read the same way.
            Col::fixed("age", 6),
            Col::fixed("status", 10),
            // "SELL ALL" is 8 characters; 7 truncated it to "SELL AL".
            Col::fixed("action", 9),
            Col::fixed("amount SOL", 13),
            Col::fixed("entry mc $", 11),
            Col::fixed("pooled", 10),
            // Full signature, last so it takes every remaining column: a
            // truncated hash cannot be pasted into an explorer, which is the
            // only reason to show it at all.
            Col::min("signature", 88),
        ],
    );
    t.empty_note = "no orders yet\npress b to buy  ·  s to sell a slice  ·  x to sell all".into();
    for o in bot.orders.iter().rev().skip(scroll).take(h) {
        let (st, tone) = match o.state {
            OrderState::Pending => ("pending", Tone::Warn),
            OrderState::Confirmed => ("confirmed", Tone::Good),
            OrderState::Failed => ("failed", Tone::Bad),
        };
        let atone = match o.action {
            "BUY" => Tone::Good,
            // Money leaving on purpose is not a loss, and colouring it red
            // beside a column of losses reads as one. A swap moves nothing out
            // of the wallet at all — it changes which asset holds it.
            "SENT" | "SWAP" => Tone::Accent,
            _ => Tone::Bad,
        };
        t.push(vec![
            Cell::new(crate::view::age_compact(o.at.elapsed().as_secs_f64())),
            Cell::bold(st, tone),
            Cell::bold(o.action, atone),
            Cell::new(format!("{:.6}", o.sol)),
            // Dollars, matching the tape's mkt cap column — the same number
            // in two currencies with no label read as a bug. SOL only when
            // no rate is known, and say so with the unit.
            Cell::new(if o.mc > 0.0 {
                if bot.sol_usd > 0.0 {
                    crate::view::usd_compact(o.mc * bot.sol_usd)
                } else {
                    format!("{:.2} SOL", o.mc)
                }
            } else {
                String::new()
            }),
            Cell::new(if o.pooled > 0.0 { format!("{:.3}", o.pooled) } else { String::new() }),
            Cell::toned(o.sig.clone().unwrap_or_default(), Tone::Normal),
        ]);
    }
    t
}

fn logs_panel(bot: &SolBot, scroll: usize, h: usize) -> PanelView {
    let (session, trace) = super::log_names();
    let mut p = PanelView::new(match (session.is_empty(), trace.is_empty()) {
        (true, true) => " Logs ".to_string(),
        (false, true) => format!(" Logs — ~/.trenches/{session} "),
        (true, false) => format!(" Logs — ~/.trenches/{trace} "),
        (false, false) => format!(" Logs — ~/.trenches/{session} · {trace} "),
    });

    // Both streams, as on the EVM side: what the bot did, and what the
    // machinery reported. Both carry an [HH:MM:SS] stamp, so a stable sort on
    // the first ten characters puts them back in the order they happened.
    let mut all: Vec<String> = bot.logs.iter().cloned().collect();
    all.extend(crate::events::recent());
    all.sort_by(|a, b| a.chars().take(10).cmp(b.chars().take(10)));

    if all.is_empty() {
        p.line_toned("no activity yet", Tone::Dim);
        return p;
    }
    for l in all.iter().rev().skip(scroll).take(h).rev() {
        // Levelled lines carry their own tone; the engine's own wording is
        // matched for the rest.
        let tone = match crate::events::Level::of(l) {
            Some(crate::events::Level::Error) => Tone::Bad,
            Some(crate::events::Level::Warn) => Tone::Warn,
            Some(crate::events::Level::Trade) => Tone::Good,
            Some(crate::events::Level::Action) => Tone::Info,
            Some(crate::events::Level::Info) => Tone::Normal,
            None if l.contains("REVERTED") || l.contains("failed") => Tone::Bad,
            None if l.contains("CONFIRMED") => Tone::Good,
            None if l.contains("SENT") => Tone::Warn,
            None => Tone::Normal,
        };
        p.line_toned(l.clone(), tone);
    }
    p
}

/// The Solana dashboard's shortcuts, from shortcuts.json — see `crate::shortcuts`. The
/// hand-written list that used to live here had drifted from the EVM one.
fn help_rows() -> Vec<(String, String)> {
    crate::shortcuts::help_rows(crate::shortcuts::Chain::Sol)
}

/// Draws the dashboard and returns where the header logo goes, so the caller
/// can place a real terminal image there after the frame.
/// Returns the header logo box and, when a coin is loaded, a small box in the
/// market panel for its artwork.
fn draw(
    f: &mut Frame,
    bot: &SolBot,
    view: Panel,
    scroll: usize,
    show_help: bool,
) -> (Option<Rect>, Option<Rect>) {
    ui::widgets::paint_bg(f);
    let c = Layout::vertical([
        Constraint::Length(5),  // header (square logo + status line)
        Constraint::Length(14), // wallet | pool
        Constraint::Length(7),  // settings — four knob rows plus the status
                                // line; at 6 the status was clipped off
        Constraint::Min(4),     // orders / tape / logs
        Constraint::Length(3),  // footer
    ])
    .split(f.area());

    // Left: the venue in large type beside its mark. Right: live chain state.
    let (logo_box, indent_cols) = ui::image::header_box(ui::widgets::themed_block("").inner(c[0]));
    // Same thresholds as the EVM header, so the two chains read alike.
    let lat = if bot.round_ms < 200.0 {
        Tone::Good
    } else if bot.round_ms < 800.0 {
        Tone::Warn
    } else {
        Tone::Bad
    };
    let name = header_venue(bot).display_name(&bot.net);
    let name_style = Style::default()
        .fg(ui::widgets::tone_color(Tone::Accent))
        .add_modifier(Modifier::BOLD);
    let avail = c[0].width.saturating_sub(indent_cols + 32);
    let head_left = if ui::bigtext::width(&name) <= avail {
        Paragraph::new(ui::bigtext::render(&name, name_style))
    } else {
        Paragraph::new(Line::from(Span::styled(name.clone(), name_style)))
    };

    let head_right = Paragraph::new(vec![
        // The chain leads: it anchors the column while the two below it change.
        Line::from(vec![Span::styled(
            format!("{} ", bot.net),
            Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
        )]),
        // Value first, label last: right-aligned, the labels line up flush
        // against the edge with the numbers beside them.
        Line::from(vec![
            Span::styled(
                format!("{:.0}ms ", bot.round_ms),
                Style::default().fg(ui::widgets::tone_color(lat)),
            ),
            Span::styled(
                "latency ",
                Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw(format!("{} ", bot.slot)),
            Span::styled(
                "slot ",
                Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .alignment(Alignment::Right);

    let head_block = ui::widgets::themed_block(format!(" Trenches Bot v{} ", crate::update::current()));
    let head_inner = head_block.inner(c[0]);
    f.render_widget(head_block, c[0]);
    let head_cols = Layout::horizontal([
        Constraint::Length(indent_cols),
        Constraint::Min(20),
        Constraint::Length(30),
    ])
    .split(head_inner);
    f.render_widget(head_left, head_cols[1]);
    f.render_widget(head_right, head_cols[2]);

    // Real image where the terminal supports one; block art is the fallback.
    if !ui::image::supported() {
        if let Some(l) = ui::logo::for_venue(header_venue(bot), &bot.net) {
            f.render_widget(
                Paragraph::new(l.render_fit(logo_box.width, logo_box.height)),
                logo_box,
            );
        }
    }

    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(c[1]);
    ui::widgets::panel(f, cols[0], &wallet_panel(bot));
    ui::widgets::panel(f, cols[1], &market_panel(bot));
    // As tall as the panel, and square: terminal cells are about twice as tall
    // as they are wide, so an `h`-row square needs `2h` columns — the same
    // reasoning `header_box` uses for the venue mark.
    //
    // Only when the text still has room to its left. A picture that overlaps
    // the mint address is worse than no picture, and on a narrow window the
    // numbers are what the panel is for.
    let coin_box = {
        let inner = ui::widgets::themed_block("").inner(cols[1]);
        // Shrink to fit, rather than vanish.
        //
        // A square `h` rows tall needs `2h` columns, so on a tall narrow panel
        // the square the height asked for was wider than the panel had — and
        // the whole box was dropped. The picture did not get smaller, it
        // stopped existing, which is what "no images on EVM" was.
        //
        // 24 columns are kept for the text, and the height follows whatever
        // width is left.
        let by_width = inner.width.saturating_sub(24) / 2;
        let h = inner.height.min(by_width);
        let w = h.saturating_mul(2);
        (h >= 3).then(|| Rect {
            x: inner.x + inner.width - w,
            y: inner.y,
            width: w,
            height: h,
        })
    };

    // Everything adjustable in one box, with the most recent message beneath.
    // Key hints and labels share the border colour; values stay normal text so
    // the number you are reading stands out against them.
    let hint = |k: &'static str| Span::styled(
        k,
        Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
    );
    // Padded so the values line up down the box rather than starting wherever
    // the label happened to end.
    let slbl_pad = |t: &str| Span::styled(
        format!("{t:<10}"),
        Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
    );
    let val = |t: String| Span::styled(t, Style::default().fg(ui::widgets::tone_color(Tone::Normal)));
    f.render_widget(
        Paragraph::new(vec![
            // Left column is what you tune while trading; right column is the
            // mode. Same grouping as the EVM settings box.
            Line::from(vec![
                hint("[ ] "),
                slbl_pad("buy"),
                val(format!("{:<14}", format!("{}% of SOL", pct(bot.buy_frac)))),
                // No key hint. `M` moves funds now, and mode was never a
                // toggle anyway — manual is the only mode there is, which is
                // the point of showing the row at all. Advertising a key that
                // does something else entirely is worse than showing none.
                hint("    "),
                slbl_pad("mode"),
                val("manual".into()),
            ]),
            Line::from(vec![
                hint("( ) "),
                slbl_pad("sell"),
                val(format!("{:.0}%", bot.sell_frac * 100.0)),
            ]),
            Line::from(vec![
                hint("{ } "),
                slbl_pad("slippage"),
                val(format!("{:.0}%", bot.slippage_pct)),
            ]),
            Line::from(vec![
                hint("< > "),
                slbl_pad("priority"),
                val(if bot.priority_auto {
                    // Show the level AND what it currently costs: the level is
                    // what you chose, the SOL is what it means right now.
                    format!("auto {} {:.6} SOL", bot.priority_level.label(), bot.priority_fee_sol())
                } else {
                    format!("{:.6} SOL", bot.priority_fee_sol())
                }),
            ]),
            Line::from(vec![
                // Not padded to the settings column: it is a message, not a
                // value in that grid, so aligning it just opens a gap.
                Span::styled(
                    "Status  ",
                    Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
                ),
                // Normal text: it is the message, not a control.
                Span::styled(bot.status.clone(), Style::default().fg(ui::widgets::tone_color(Tone::Info))),
            ]),
        ])
        .block(ui::widgets::themed_block(" Settings ")),
        c[2],
    );

    let h = c[3].height.saturating_sub(3).max(1) as usize;
    match view {
        Panel::Orders => {
            let mut tv = orders_table(bot, scroll, h);
            tv.active_key = Some('o');
            ui::widgets::table(f, c[3], &tv, None)
        }
        Panel::Tape => {
            let mut tv = discover::tape_view(&bot.tape, scroll, h, bot.sol_usd);
            tv.active_key = Some('t');
            ui::widgets::table(f, c[3], &tv, None)
        }
        Panel::Logs => {
            let mut pv = logs_panel(bot, scroll, h + 1);
            pv.active_key = Some('l');
            ui::widgets::panel(f, c[3], &pv)
        }
        Panel::Chart => {
            let mut cv = chart_view(bot);
            cv.active_key = Some('c');
            ui::widgets::candles(f, c[3], &cv)
        }
    }

    let key = |k: &'static str, t: Tone| {
        Span::styled(k, Style::default().fg(ui::widgets::tone_color(t)).add_modifier(Modifier::BOLD))
    };
    // System keys wear the frame's own colour, like the panel menu's inactive
    // keys — only the trade keys carry tones, so colour means action.
    let sys = |k: &'static str| {
        Span::styled(k, Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD))
    };
    let mut keys: Vec<Span> = vec![
        key("[b]", Tone::Good),
        Span::raw(" buy  "),
        key("[s]", Tone::Bad),
        Span::raw(" sell  "),
        key("[x]", Tone::Bad),
        Span::raw(" sell-all  "),
        key("[f]", Tone::Accent),
        Span::raw(" find  "),

        sys("[T]"),
        Span::raw(" theme  "),
        sys("[?]"),
        Span::raw(" help  "),
        sys("[C]"),
        Span::raw(" chain  "),
        sys("[W]"),
        Span::raw(" wallet  "),
        sys("[D]"),
        Span::raw(" docs  "),
        sys("[q]"),
        Span::raw(" quit"),
    ];

    // The build, against the right edge — the same as the EVM dashboard, so a
    // bug report from either chain can name what was running. Dropped rather
    // than wrapped on a narrow terminal: the keys are what the footer is for.
    // Same prompt as the EVM footer: the key on the left when there is
    // something to install, and "(latest)" on the right when there is not.
    if let Some(v) = crate::update::available() {
        keys.push(Span::styled(
            "  [U]",
            Style::default().fg(ui::widgets::tone_color(Tone::Info)).add_modifier(Modifier::BOLD),
        ));
        keys.push(Span::styled(
            format!(" update to {v}"),
            Style::default().fg(ui::widgets::tone_color(Tone::Info)),
        ));
    }
    let build = crate::update::footer_label();
    let used: usize = keys.iter().map(|s| s.content.chars().count()).sum();
    let inner = c[4].width.saturating_sub(2) as usize;
    if inner > used + build.chars().count() + 2 {
        keys.push(Span::raw(" ".repeat(inner - used - build.chars().count())));
        keys.push(Span::styled(
            build,
            Style::default().fg(ui::widgets::tone_color(Tone::Dim)),
        ));
    }
    let footer = Paragraph::new(Line::from(keys)).block(ui::widgets::themed_block(""));
    f.render_widget(footer, c[4]);

    if show_help {
        let rows = help_rows();
        let items: Vec<(&str, &str)> =
            rows.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        ui::widgets::help(f, &items, " Shortcuts  (any key to close) ");
    }
    (Some(logo_box), coin_box)
}

/// With a coin loaded the venue is its pump.fun launch; with none selected
/// there is no venue yet, so the chain's own mark is the honest thing to show.
fn header_venue(bot: &SolBot) -> ui::image::Venue {
    if bot.coin.is_some() {
        ui::image::Venue::PumpFun
    } else {
        ui::image::Venue::Chain
    }
}

// ---- trenches screen -----------------------------------------------------

/// The most launches the trenches cache holds. Newest win: a launchpad list
/// is about what is happening now, not an archive.
const TRENCH_MAX: usize = 150;

/// The discovered launches, alive for the whole process so leaving the screen
/// and coming back does not start from an empty feed.
fn trench_cache() -> Arc<Mutex<Vec<TrenchRow>>> {
    static CACHE: std::sync::OnceLock<Arc<Mutex<Vec<TrenchRow>>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default).clone()
}

/// What the launch feed is doing, process-wide — the screen only reports it.
fn feed_status() -> Arc<Mutex<String>> {
    static ST: std::sync::OnceLock<Arc<Mutex<String>>> = std::sync::OnceLock::new();
    ST.get_or_init(|| Arc::new(Mutex::new("connecting to the launch feed…".to_string()))).clone()
}

/// Start the launch feed ONCE for the process, and leave it running.
///
/// It used to be spawned by the trenches screen and aborted on the way out, so
/// the moment you left to trade something the feed stopped. The row cache
/// survived, which made this hard to see — you came back to everything you had
/// found and assumed nothing had happened while you were gone. But the
/// websocket only reports launches that occur AFTER it subscribes, so every
/// launch during a trade was missed permanently. Nothing could backfill it.
///
/// Now it outlives the screen, and the screen is a view onto a feed that never
/// stopped. Idempotent: called on every entry, subscribes only the first time.
pub fn ensure_launch_feed(rpc: &Rpc, ws_urls: &[String]) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.get().is_some() {
        return;
    }
    let _ = STARTED.set(());
    let (found, status) = (trench_cache(), feed_status());
    let (rpc2, ws2) = (rpc.clone(), ws_urls.to_vec());
    tokio::spawn(async move {
        let st2 = status.clone();
        let r = discover::watch_launches_ha(
            &rpc2,
            &ws2,
            |launch| {
                let (f3, rpc3) = (found.clone(), rpc2.clone());
                super::trace(&format!("launch {}", launch.mint));
                tokio::spawn(async move {
                    let rows = discover::enrich(&rpc3, vec![launch]).await;
                    super::trace(&format!("enrich -> {} row(s)", rows.len()));
                    let mut cur = f3.lock().unwrap();
                    for row in rows {
                        // The cache outlives the screen, so the same mint can
                        // arrive again — refresh it in place, don't double it.
                        match cur.iter_mut().find(|r| r.launch.mint == row.launch.mint) {
                            Some(slot) => *slot = row,
                            None => cur.push(row),
                        }
                    }
                    // Bounded: the list only ever grew, and the sweep re-reads
                    // every row it holds — an afternoon of launches made each
                    // pass cost more than the last, forever.
                    if cur.len() > TRENCH_MAX {
                        discover::sort_newest_first(&mut cur);
                        cur.truncate(TRENCH_MAX);
                    }
                });
                true // runs for the life of the session
            },
            move |msg| {
                super::trace(&format!("feed: {msg}"));
                if let Ok(mut f) = st2.lock() {
                    *f = msg;
                }
            },
        )
        .await;
        // A failed websocket must not look like an idle one — that read as
        // "watching…" forever with no hint anything was wrong.
        if let Err(e) = r {
            super::trace(&format!("feed DOWN: {e}"));
            if let Ok(mut f) = status.lock() {
                *f = format!("launch feed down: {e}");
            }
        }
    });
}

async fn screen_trenches(
    term: &mut Term,
    rpc: &Rpc,
    ws_urls: &[String],
    sol_usd: f64,
    risk: &super::rugcheck::RugCheck,
    warn_score: u32,
) -> eyre::Result<Option<(Pubkey, Option<i64>, String)>> {
    // This screen owns the terminal now: take down the dashboard's image, which
    // also marks it stale so it redraws when we come back.
    ui::image::clear();
    // The launches live in a process-wide cache, NOT on this screen's stack
    // frame. They used to die with the screen: pick a coin, trade it, come
    // back — and everything the feed had found was gone, unrecoverable until
    // a brand-new mint happened to launch, because the websocket feed only
    // reports launches that happen AFTER it subscribes. Now returning shows
    // everything found before, instantly.
    // The feed is already running — started with the session, not with this
    // screen — so this is a VIEW onto it. Leaving no longer stops it.
    ensure_launch_feed(rpc, ws_urls);
    let found: Arc<Mutex<Vec<TrenchRow>>> = trench_cache();
    let feed: Arc<Mutex<String>> = feed_status();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Keep the visible rows live. Without this a coin discovered at 20% bonded
    // showed 20% forever while it actually filled — the one number that says
    // "this launch is working" was frozen at discovery.
    // [R] asks for a sweep now; `refreshing` says one is in flight. Without the
    // second, a refresh on a slow endpoint is indistinguishable from a screen
    // that has stopped caring — you press R and nothing visibly happens.
    let refresh_now = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refreshing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (f4, s4, rpc4) = (found.clone(), stop.clone(), rpc.clone());
    let (rn4, rf4) = (refresh_now.clone(), refreshing.clone());
    let refresher = tokio::spawn(async move {
        while !s4.load(std::sync::atomic::Ordering::Relaxed) {
            // Sliced, so [R] does not wait out the rest of the interval.
            for _ in 0..30u32 {
                if s4.load(std::sync::atomic::Ordering::Relaxed)
                    || rn4.swap(false, std::sync::atomic::Ordering::Relaxed)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let mints: Vec<Pubkey> = {
                let rows = f4.lock().unwrap();
                rows.iter().map(|r| r.launch.mint).collect()
            };
            if mints.is_empty() {
                continue;
            }
            rf4.store(true, std::sync::atomic::Ordering::Relaxed);
            let curves: Vec<Pubkey> = mints.iter().map(super::bonding_curve_pda).collect();
            let mut fresh: Vec<Option<super::pumpfun::BondingCurve>> = Vec::new();
            // 100 keys per request is the RPC's hard cap.
            for chunk in curves.chunks(100) {
                match rpc4.accounts(chunk).await {
                    Ok(accs) => fresh.extend(
                        accs.into_iter()
                            .map(|d| d.and_then(|b| super::pumpfun::BondingCurve::decode(&b).ok())),
                    ),
                    Err(e) => {
                        super::trace(&format!("trenches refresh failed: {e}"));
                        fresh.resize(curves.len(), None);
                        break;
                    }
                }
            }
            // Re-match by mint: the list grows while the request is in flight,
            // so index-aligning against the current rows would shift metrics
            // onto the wrong coins.
            let mut rows = f4.lock().unwrap();
            for (mint, curve) in mints.iter().zip(fresh) {
                let Some(curve) = curve else { continue };
                if let Some(row) = rows.iter_mut().find(|r| r.launch.mint == *mint) {
                    row.curve = curve;
                }
            }
            drop(rows);
            rf4.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    });

    let mut cursor = ui::widgets::Cursor::new();
    let mut msel = ui::mouse::Selection::default();
    let mut copy_armed = false;
    let result = loop {
        let mut rows = found.lock().unwrap().clone();
        discover::sort_newest_first(&mut rows);
        // Warm one unknown report per pass — SPAWNED, because this is the draw
        // loop: awaiting the HTTP fetch here held the whole screen (keys,
        // rendering, everything) hostage to RugCheck's latency, up to 6s per
        // new row. "Unknown", not "uncached": a recent failure counts as an
        // answer, so one broken mint cannot pin the warm slot forever.
        if let Some(r) = rows.iter().find(|r| !risk.known(&r.launch.mint.to_string())) {
            let (rk, mint) = (risk.clone(), r.launch.mint.to_string());
            tokio::spawn(async move {
                let _ = rk.report(&mint).await;
            });
        }
        let mut table = discover::table_view(&rows, sol_usd, Some(risk), warn_score);
        // Say out loud whether a sweep is running. A refresh you cannot see is
        // the same complaint as a screen that never refreshes.
        let busy = refreshing.load(std::sync::atomic::Ordering::Relaxed);
        table.title = if busy {
            format!("{}  ·  refreshing…", table.title.trim_end())
        } else {
            format!("{}  ·  [R] refresh", table.title.trim_end())
        };
        // Replace the generic note with what the feed is actually doing.
        if rows.is_empty() {
            let status = feed.lock().map(|f| f.clone()).unwrap_or_default();
            let hint = if busy { "refreshing now…" } else { "[R] to search again" };
            table.empty_note = format!("{status}\nnew launches appear the moment they are created  ·  {hint}  ·  esc to go back");
        }
        let n = rows.len();
        let mut grabbed: Option<String> = None;
        term.draw(|f| {
            ui::widgets::paint_bg(f);
            let st = cursor.state_for(n);
            ui::widgets::table(f, f.area(), &table, Some(st));
            ui::mouse::paint(f, &msel);
            if copy_armed {
                if let Some((a, b)) = msel.region() {
                    grabbed = Some(ui::mouse::selected_text(f.buffer_mut(), a, b));
                }
            }
        })?;
        if let Some(t) = grabbed {
            copy_armed = false;
            msel.clear();
            if !t.is_empty() {
                ui::mouse::copy(&t);
            }
        }

        crate::ui_alive();
        crate::ui_phase_set("the Solana screen, waiting for a key");

        if event::poll(Duration::from_millis(150))? {
            let evt = event::read()?;
            if let Event::Mouse(m) = evt {
                if msel.on_mouse(m) {
                    copy_armed = true;
                }
            }
            if let Event::Key(k) = evt {
                if !crate::ui::fresh_key(k.code) { continue; }
                // Ask for a sweep now rather than waiting out the interval.
                if matches!(k.code, KeyCode::Char('r') | KeyCode::Char('R')) {
                    refresh_now.store(true, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                match cursor.on_key(k.code, n) {
                    ui::widgets::Nav::Enter => break rows
                        .get(cursor.sel)
                        .map(|r| (r.launch.mint, r.launch.block_time, r.launch.signature.clone())),
                    ui::widgets::Nav::Back => break None,
                    _ => {}
                }
            }
        }
    };

    // Only the screen's own work stops here. The launch feed keeps running —
    // that is the entire point: leaving to trade must not cost you the
    // launches that happen while you are gone.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    refresher.abort();
    Ok(result)
}

// ---- main loop -----------------------------------------------------------

pub async fn run(
    term: &mut Term,
    rpc_urls: Vec<String>,
    // Every explicitly-configured WS endpoint (`ws` + `wss`), primary first.
    ws_explicit: Vec<String>,
    signer: Keypair,
    // False when nobody unlocked a keystore: the signer is a throwaway and must
    // never be asked to sign. Reads work; the order keys do not.
    has_account: bool,
    rugcheck: &crate::config::RugCheck,
    net: &str,
) -> eyre::Result<crate::Exit> {
    let rpc = Rpc::new_pool(rpc_urls.clone());
    // Start listening for launches NOW, not when the trenches screen is first
    // opened — otherwise the feed misses everything that happens before you
    // think to look, which is most of them.
    ensure_launch_feed(&rpc, &ws_explicit);
    // Providers frequently host WS on a separate domain, so explicit settings
    // win over deriving from the HTTP URLs.
    // Candidates in preference order: every explicit endpoint first (the feed
    // rotates through them on a drop or error), then one derived from each RPC
    // endpoint. Providers host WS on a separate domain often enough that
    // deriving alone isn't reliable, but having the derived ones as fallbacks
    // means a single provider outage can't blind the feed.
    let mut ws_urls: Vec<String> = ws_explicit;
    for u in &rpc_urls {
        let d = discover::ws_url_from_http(u);
        if !d.is_empty() && !ws_urls.contains(&d) {
            ws_urls.push(d);
        }
    }
    super::trace(&format!(
        "session start: net={net} rpc x{} ws x{}",
        rpc_urls.len(),
        ws_urls.len()
    ));
    let mut bot = SolBot::new(rpc, signer, rugcheck, net);
    bot.has_account = has_account;
    if !has_account {
        bot.status = "no account — press [W] to unlock one".into();
    }
    let mut exit = crate::Exit::Quit;
    // Header logo. Screens that take over clear images on entry, which marks
    // this stale, so it redraws on return without bookkeeping here.
    let mut chain_logo = ui::image::Placement::default();
    let mut coin_art = ui::image::Placement::default();
    let mut show_help = false;
    let mut view = Panel::Orders;
    let mut scroll: usize = 0;
    let mut last_reap = Instant::now();
    // Force an immediate first refresh.
    // Background poller owns all timed RPC; the UI thread only reads snapshots.
    let snap: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(Snapshot::default()));
    let target: Arc<Mutex<Option<PollTarget>>> = Arc::new(Mutex::new(None));
    // A coin being resolved in the BACKGROUND. Pasting an address used to
    // await load_coin + metadata on the UI thread — five to ten round trips
    // of frozen screen. Now the keypress spawns the work and the loop adopts
    // the coin the pass after it lands; the screen never stops.
    struct PendingCoin {
        mint: Pubkey,
        launched: Option<i64>,
        seed: Vec<discover::SolSwap>,
        meta: Option<super::metadata::TokenMeta>,
        coin: eyre::Result<engine::Coin>,
    }
    // Learn what the wallet holds before anyone presses M — see the same
    // warm-up on the EVM dashboard for why.
    if bot.has_account {
        let (rpc2, me2) = (bot.rpc.clone(), bot.trader());
        tokio::spawn(async move {
            let fresh = read_wallet_assets(&rpc2, &me2).await;
            if let Ok(mut g) = wallet_assets().lock() {
                *g = Some(fresh);
            }
            mark_read();
        });
    }

    let pending_coin: Arc<Mutex<Option<PendingCoin>>> = Default::default();
    let spawn_resolve = {
        let rpc = bot.rpc.clone();
        let cell = pending_coin.clone();
        let trader = bot.trader();
        move |mint: Pubkey, launched: Option<i64>, launch_sig: Option<String>| {
            let (rpc, cell) = (rpc.clone(), cell.clone());
            tokio::spawn(async move {
                // A couple of bounded retries: session start resolves the
                // restored coin while every RPC is still cold, and one
                // transient refusal used to cost the whole restore — the
                // dashboard came up empty and the mint had to be re-pasted.
                let mut coin = engine::load_coin(&rpc, &mint).await;
                for wait_ms in [700u64, 1500] {
                    if coin.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                    coin = engine::load_coin(&rpc, &mint).await;
                }
                let meta = super::metadata::token_meta(&rpc, &mint).await;
                let seed = match &launch_sig {
                    Some(sig) => discover::tape_seed(&rpc, sig, &mint, &trader).await,
                    None => Vec::new(),
                };
                *cell.lock().unwrap() = Some(PendingCoin { mint, launched, seed, meta, coin });
            });
        }
    };

    // Pick up where the last session (or the pre-wallet-switch session)
    // left off: the last coin resolves in the background, its saved tape
    // loads with it, and the dashboard is exactly as it was left.
    if let Some(mint) = load_last_coin() {
        bot.note(format!("restoring {mint}…"));
        spawn_resolve(mint, None, None);
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (rows_tx, rows_rx) =
        tokio::sync::mpsc::unbounded_channel::<(Pubkey, String, Vec<discover::SolSwap>)>();
    bot.fills_tx = Some(rows_tx.clone());
    let poll_handle = tokio::spawn(poller(
        bot.rpc.clone(),
        bot.trader(),
        ws_urls.clone(),
        target.clone(),
        snap.clone(),
        stop.clone(),
        rows_tx,
        rows_rx,
    ));

    let mut msel = ui::mouse::Selection::default();
    let mut copy_armed = false;
    loop {
        let mut logo_box = None;
        let mut coin_box = None;
        let mut grabbed: Option<String> = None;
        term.draw(|f| {
            let (l, c) = draw(f, &bot, view, scroll, show_help);
            logo_box = l;
            coin_box = c;
            ui::mouse::paint(f, &msel);
            if copy_armed {
                if let Some((a, b)) = msel.region() {
                    grabbed = Some(ui::mouse::selected_text(f.buffer_mut(), a, b));
                }
            }
        })?;
        if let Some(t) = grabbed {
            copy_armed = false;
            msel.clear();
            if !t.is_empty() {
                ui::mouse::copy(&t);
                bot.note(format!("copied {} characters", t.chars().count()));
            }
        }
        let venue = header_venue(&bot);
        let term_size = term.size().map(|s| (s.width, s.height)).unwrap_or((0, 0));
        if let (Some(r), Some(png)) = (logo_box, ui::image::for_venue(venue, &bot.net)) {
            chain_logo.show(png, venue as usize, r.x, r.y, r.width, r.height, term_size);
        }
        // The coin's own art, if the fetch has landed. Keyed on the mint so
        // switching coins redraws rather than leaving the last one up.
        match (coin_box, bot.coin.as_ref()) {
            (Some(r), Some(c)) => {
                let url = bot.meta.as_ref().and_then(|m| m.image.as_deref()).unwrap_or("");
                crate::art::request(url);
                match crate::art::cached(url) {
                    Some(png) => {
                        let id =
                            c.mint.to_bytes()[..8].iter().fold(0usize, |a, b| a << 8 | *b as usize);
                        coin_art.show(png.as_slice(), id, r.x, r.y, r.width, r.height, term_size);
                    }
                    None => coin_art.hide(),
                }
            }
            _ => coin_art.hide(),
        }

        crate::ui_alive();
        crate::ui_phase_set("the Solana dashboard, waiting for a key");

        if event::poll(Duration::from_millis(100))? {
            let evt = event::read()?;
            if let Event::Mouse(m) = evt {
                use crossterm::event::MouseEventKind as MK;
                match m.kind {
                    // Wheel scrolls the active panel, three rows per notch.
                    MK::ScrollUp => {
                        let n = match view {
                            Panel::Logs => bot.logs.len(),
                            Panel::Tape => bot.tape.len(),
                            Panel::Orders => bot.orders.len(),
                            Panel::Chart => 0,
                        };
                        scroll = (scroll + 3).min(n.saturating_sub(1));
                    }
                    MK::ScrollDown => scroll = scroll.saturating_sub(3),
                    _ => {
                        if msel.on_mouse(m) {
                            copy_armed = true; // extracted on the next frame
                        }
                    }
                }
            }
            if let Event::Key(k) = evt {
                if !crate::ui::fresh_key(k.code) { continue; }
                // Any await a key arm does holds the UI; name the key so a
                // freeze report says which action was responsible.
                crate::ui_phase_set(&format!("the {:?} key's action on Solana", k.code));
                if show_help {
                    show_help = false;
                    if k.code == KeyCode::Char('?') {
                        continue;
                    }
                }
                // No account: nothing can be signed, and the throwaway key that
                // built the client must never be asked to try. Reads carry on —
                // watching the tape without a key on the machine is a reasonable
                // thing to want.
                if !bot.has_account && matches!(k.code, KeyCode::Char('b' | 's' | 'x')) {
                    bot.status = "no account — press [W] to unlock one".into();
                    continue;
                }
                match k.code {
                    // `q`/esc ask first; `Q` quits outright.
                    KeyCode::Char('q') | KeyCode::Esc => {
                        if ui::confirm(term, "Quit?")? {
                            break;
                        }
                    }
                    KeyCode::Char('Q') => break,
                    // Same as the EVM side: the only path that installs
                    // anything, and it asks first.
                    KeyCode::Char('U') => match crate::update::status() {
                        crate::update::Status::Checking => {
                            bot.note("Still checking for updates…".to_string())
                        }
                        crate::update::Status::Unknown => {
                            bot.note("Could not reach GitHub to check for updates.".to_string())
                        }
                        crate::update::Status::Soaking { tag, ready_in } => bot.note(format!(
                            "{tag} was published recently. It will be offered in {} — set TRENCHES_UPDATE_NOW=1 to take it now.",
                            crate::update::short_hours(ready_in)
                        )),
                        crate::update::Status::Latest => bot.note(format!(
                            "You are on the latest version ({}).",
                            crate::update::full()
                        )),
                        crate::update::Status::Update(v) => {
                            if ui::confirm(term, &format!("Update to {v}?"))? {
                                crate::events::action("Updating", &[("to", v.clone())]);
                                bot.note(format!("Installing {v}…"));
                                match crate::update::install_latest().await {
                                    Ok(msg) => {
                                        crate::events::action("Update installed", &[("version", v)]);
                                        bot.note(msg);
                                    }
                                    Err(why) => {
                                        crate::events::error("Update failed", &[("reason", why.clone())]);
                                        bot.note(why);
                                    }
                                }
                            }
                        }
                    },
                    KeyCode::Char('D') => {
                        crate::events::action("Opened docs", &[("chain", "solana".to_string())]);
                        ui::docs(term)?
                    }
                    // Back to the wallet list on this same chain, as on EVM.
                    // `w` is an unlisted alias for `W`, as on the EVM side.
                    KeyCode::Char('W') | KeyCode::Char('w') => {
                        exit = crate::Exit::ChangeAccount;
                        break;
                    }
                    // Back to the chain picker without restarting the binary.
                    KeyCode::Char('C') => {
                        exit = crate::Exit::ChangeChain;
                        break;
                    }
                    // Move SOL or a token to another address.
                    //
                    // Built here, not routed: a transfer is the one operation
                    // whose whole transaction is knowable in advance. It is
                    // also the one with no undo, which is why every step below
                    // asks rather than assumes.
                    KeyCode::Char('M') => {
                        if !bot.has_account {
                            bot.note("Unlock an account first — press W".to_string());
                        } else if let Err(e) = send_flow(term, &mut bot).await {
                            bot.note(format!("Move cancelled. {e}"));
                        }
                    }
                    // USDC into SOL, or back. `u` for USDC — `U` was already
                    // the update check, which is the kind of collision the
                    // shortcuts file exists to surface.
                    KeyCode::Char('u') => {
                        if !bot.has_account {
                            bot.note("Unlock an account first — press W".to_string());
                        } else if let Err(e) = swap_flow(term, &mut bot).await {
                            bot.note(format!("Swap cancelled. {e}"));
                        }
                    }
                    // Price or market cap — one series, two units. Same key as
                    // the EVM chart, because it is the same question.
                    KeyCode::Char('m') => {
                        bot.chart_mcap = !bot.chart_mcap;
                        bot.note(if bot.chart_mcap {
                            "chart: market cap".to_string()
                        } else {
                            "chart: price".to_string()
                        });
                    }
                    // The panel reads in this currency, so the key that
                    // changes it belongs on this dashboard too.
                    KeyCode::Char('$') => {
                        bot.status = "loading currencies…".into();
                        match ui::currency_picker(term).await? {
                            Some(note) => bot.note(note),
                            None => bot.status = "ready".into(),
                        }
                    }
                    // The full address, on the clipboard — see the EVM side.
                    KeyCode::Char('y') => {
                        if !bot.has_account {
                            bot.note("No account — press [W] to unlock one".to_string());
                        } else {
                            let a = bot.trader().to_string();
                            ui::mouse::copy(&a);
                            bot.note(format!("Copied {a}"));
                        }
                    }
                    KeyCode::Char('?') => show_help = true,
                    KeyCode::Char('T') => match ui::widgets::theme_picker(term)? {
                        Some(name) => bot.note(format!("Changed theme to {name}")),
                        None => bot.note("theme unchanged"),
                    },
                    // The PnL calendar. Solana sells were already writing the
                    // ledger — the key to LOOK at it only existed on the EVM
                    // dashboard, which read as "Solana has no PnL".
                    KeyCode::Char('L') => {
                        crate::pnl::screen(term)?;
                    }
                    // Direct panel keys, matching the EVM dashboard: a panel
                    // is a destination, not a stop on a carousel.
                    KeyCode::Char('t') => {
                        view = Panel::Tape;
                        scroll = 0;
                    }
                    KeyCode::Char('o') => {
                        view = Panel::Orders;
                        scroll = 0;
                    }
                    KeyCode::Char('l') => {
                        view = Panel::Logs;
                        scroll = 0;
                    }
                    // Capital O spins the carousel; lowercase keys jump direct.
                    KeyCode::Char('O') | KeyCode::Right => {
                        view = match view {
                            Panel::Orders => Panel::Tape,
                            Panel::Tape => Panel::Chart,
                            Panel::Chart => Panel::Logs,
                            Panel::Logs => Panel::Orders,
                        };
                        scroll = 0;
                    }
                    KeyCode::Left => {
                        view = match view {
                            Panel::Orders => Panel::Logs,
                            Panel::Logs => Panel::Chart,
                            Panel::Chart => Panel::Tape,
                            Panel::Tape => Panel::Orders,
                        };
                        scroll = 0;
                    }
                    // Straight to the chart, and , . walk the candle interval.
                    KeyCode::Char('c') | KeyCode::Char('v') => {
                        view = Panel::Chart;
                        scroll = 0;
                    }
                    // Status only, not note(): stepping through six intervals
                    // is browsing, not an event — it was filling the log ring
                    // with a "candles: 5s" line per keypress.
                    KeyCode::Char(',') => {
                        bot.chart_iv = crate::view::iv_step(bot.chart_iv, false);
                        bot.status = format!("candles: {}", crate::view::iv_label(bot.chart_iv));
                    }
                    KeyCode::Char('.') => {
                        bot.chart_iv = crate::view::iv_step(bot.chart_iv, true);
                        bot.status = format!("candles: {}", crate::view::iv_label(bot.chart_iv));
                    }
                    KeyCode::Up => {
                        let n = match view {
                            Panel::Logs => bot.logs.len(),
                            Panel::Tape => bot.tape.len(),
                            Panel::Orders => bot.orders.len(),
                            Panel::Chart => 0,
                        };
                        scroll = (scroll + 1).min(n.saturating_sub(1));
                    }
                    KeyCode::Down => scroll = scroll.saturating_sub(1),
                    KeyCode::Char(']') => {
                        let step = buy_step(&bot);
                        // Snap to the step's grid, so a size set under a coarse
                        // step does not leave every later press landing on
                        // 1.35%, 1.45%, 1.55%.
                        bot.buy_frac = (((bot.buy_frac / step).round() + 1.0) * step).min(1.0);
                        bot.note(format!("Buy size is now {}% of your SOL", pct(bot.buy_frac)));
                    }
                    KeyCode::Char('[') => {
                        let step = buy_step(&bot);
                        bot.buy_frac = (((bot.buy_frac / step).round() - 1.0) * step).max(step);
                        bot.note(format!("Buy size is now {}% of your SOL", pct(bot.buy_frac)));
                    }
                    // The step itself, as on the EVM dashboard.
                    KeyCode::Char(';') => nudge_buy_step(&mut bot, false),
                    KeyCode::Char('\'') => nudge_buy_step(&mut bot, true),
                    // [] buy · () sell · {} slippage · <> priority, matching
                    // the header labels and the EVM dashboard.
                    KeyCode::Char(')') => {
                        bot.sell_frac = (bot.sell_frac + 0.10).min(1.0);
                        bot.note(format!("Sell size is now {:.0} percent of your balance", bot.sell_frac * 100.0));
                    }
                    KeyCode::Char('(') => {
                        bot.sell_frac = (bot.sell_frac - 0.10).max(0.10);
                        bot.note(format!("Sell size is now {:.0} percent of your balance", bot.sell_frac * 100.0));
                    }
                    KeyCode::Char('}') => bot.slippage_pct = (bot.slippage_pct + 1.0).min(50.0),
                    KeyCode::Char('{') => bot.slippage_pct = (bot.slippage_pct - 1.0).max(1.0),
                    // Floored at 1: integer halving reaches 0, and doubling 0
                    // stays 0 — priority would be stuck off with no way back.
                    KeyCode::Char('>') => {
                        if bot.priority_auto {
                            bot.priority_level = bot.priority_level.step(true);
                            bot.note(format!("Priority is now {}", bot.priority_level.label()));
                        } else {
                            bot.cu_price_micro = (bot.cu_price_micro.max(1) * 2).min(10_000_000);
                            bot.note(format!("Priority fee is now {:.6} SOL per trade", bot.priority_fee_sol()));
                        }
                    }
                    KeyCode::Char('<') => {
                        if bot.priority_auto {
                            bot.priority_level = bot.priority_level.step(false);
                            bot.note(format!("Priority is now {}", bot.priority_level.label()));
                        } else {
                            bot.cu_price_micro = (bot.cu_price_micro / 2).max(1);
                            bot.note(format!("Priority fee is now {:.6} SOL per trade", bot.priority_fee_sol()));
                        }
                    }
                    // Auto priority: track what competitive traffic is paying on
                    // THIS coin's accounts rather than guessing by hand.
                    KeyCode::Char('P') => {
                        bot.priority_auto = !bot.priority_auto;
                        bot.note(if bot.priority_auto {
                            format!("Priority follows the network at {}", bot.priority_level.label())
                        } else {
                            format!("Priority is fixed at {:.6} SOL per trade", bot.priority_fee_sol())
                        });
                    }
                    // Paste a mint directly — the counterpart to the EVM 'p'.
                    // Useful when you have an address from elsewhere and don't
                    // want to wait for it to appear in the live feed.
                    KeyCode::Char('p') => {
                        // A picker, matching the EVM side's `p`: paste first,
                        // then everything already loaded this session.
                        let mut labels = vec!["＋ Add token by contract address".to_string()];
                        labels.extend(bot.coins.iter().map(|c| c.label.clone()));
                        let choice = ui::select(term, "Coins", &labels)?;
                        let picked = match choice {
                            None => None,                                  // esc
                            Some(0) => Some(None),                         // paste
                            Some(i) => Some(bot.coins.get(i - 1).map(|c| c.mint)),
                        };
                        let typed = match picked {
                            None => None,
                            // An already-loaded coin: skip the prompt entirely.
                            Some(Some(mint)) => Some(mint.to_string()),
                            Some(None) => ui::input(
                                term,
                                "Add token by contract address",
                                "paste the CA (base58) — enter to load, esc to cancel",
                            )?,
                        };
                        if let Some(txt) = typed {
                            let txt = txt.trim().to_string();
                            match txt.parse::<Pubkey>() {
                                // Base58 is CASE-SENSITIVE. A lowercased address
                                // is the most common paste failure and produces a
                                // useless "invalid" otherwise, so name it.
                                Err(_) if txt == txt.to_lowercase() && txt.len() > 30 => bot.note(
                                    "That address looks lowercased. Solana addresses are case sensitive, so paste the original",
                                ),
                                Err(_) => bot.note(format!("That is not a valid Solana address. It has {} characters", txt.len())),
                                Ok(mint) => {
                                    // Resolution runs in the BACKGROUND; the
                                    // loop adopts the coin when it lands. The
                                    // screen keeps drawing the whole time.
                                    bot.note(format!("resolving {mint}…"));
                                    spawn_resolve(mint, None, None);
                                }
                            }
                        }
                    }
                    KeyCode::Char('f') => {
                        bot.note("Watching for new launches");
                        if let Some((mint, launched, launch_sig)) = screen_trenches(term, &bot.rpc, &ws_urls, bot.sol_usd, &bot.risk, bot.warn_score).await? {
                            // Background resolution, launch-tx tape seed
                            // included; the loop adopts it when it lands.
                            bot.note(format!("resolving {mint}…"));
                            spawn_resolve(mint, launched, Some(launch_sig));
                        } else {
                            bot.note("cancelled");
                        }
                    }
                    KeyCode::Char('b') => {
                        let (sol, slip, cu) = (bot.buy_size_sol(), bot.slippage_pct, bot.cu_price_micro);
                        if bot.coin.is_none() {
                            bot.note("No coin is selected");
                        } else if sol <= 0.0 {
                            // Name the REASON, not the symptom. "buy size must
                            // be > 0" sent you looking at the percentage, when
                            // the percentage was never the problem: the balance
                            // is under the reserve, so every size is zero.
                            let need = bot.buy_reserve_sol();
                            bot.note(format!(
                                "Balance {:.6} SOL is under the {need:.6} SOL a buy must leave for fees and token-account rent. Add SOL to trade",
                                bot.sol
                            ));
                        } else if sol > bot.sol {
                            // Refuse locally rather than burning a fee on a
                            // transaction the node will reject anyway.
                            bot.note(format!("Not enough SOL. You have {:.6} and need {sol:.6}", bot.sol));
                        } else {
                            bot.note(format!("Sending a buy for {sol:.6} SOL"));
                            // Re-read the venue's reserves FIRST: the poller's
                            // snapshot is up to 1.5s old, a launch moves >5%/s,
                            // and a floor computed on a stale price fails
                            // preflight with BuySlippageBelowMinTokensOut. One
                            // bounded read prices the trade on the present.
                            if let Some(c) = bot.coin.as_mut() {
                                let _ = tokio::time::timeout(
                                    Duration::from_millis(900),
                                    engine::refresh_venue(&bot.rpc, c),
                                )
                                .await;
                            }
                            let sent = {
                                let coin = bot.coin.as_ref().expect("checked above");
                                engine::buy(&bot.rpc, &bot.signer, coin, sol, slip, cu).await
                            };
                            match sent {
                                Ok(sig) => {
                                    bot.push_order("BUY", sol, 0.0, OrderState::Pending, Some(sig.clone()));
                                    bot.note(format!("Buy for {sol:.6} SOL sent, signature {sig}"));
                                }
                                Err(e) => {
                                    bot.push_order("BUY", sol, 0.0, OrderState::Failed, None);
                                    bot.fails += 1;
                                    bot.note(format!("Buy failed. {e}"));
                                }
                            }
                        }
                    }
                    KeyCode::Char('s') | KeyCode::Char('x') => {
                        // `x` always liquidates; `s` sells the configured slice.
                        let all = matches!(k.code, KeyCode::Char('x'));
                        let frac = if all { 1.0 } else { bot.sell_frac };
                        // The balance the POLL knows is a read old — but our own
                        // fills are on the tape within a second of confirming.
                        // A buy you just made IS something to sell: size from
                        // the larger of the two and let engine::sell clamp to
                        // the wallet's actual units at send time. This is the
                        // race that kept "There is nothing to sell" on screen
                        // for ten seconds after a confirmed entry.
                        let held = bot.effective_tokens().max(bot.tape_net_tokens());
                        let (tokens, slip, cu) =
                            (held * frac, bot.slippage_pct, bot.cu_price_micro);
                        // One liquidation in flight at a time: a second x while
                        // the first is pending burned a fee on a guaranteed
                        // revert — the wallet it would read is already empty.
                        let sell_pending = bot
                            .orders
                            .iter()
                            .any(|o| o.action == "SELL" && o.state == OrderState::Pending);
                        if bot.coin.is_none() {
                            bot.note("No coin is selected");
                        } else if sell_pending {
                            bot.note("A sell is already in flight — waiting for it to land");
                        } else if tokens <= 0.0 {
                            bot.note("There is nothing to sell");
                        } else {
                            // Quote the exit from reserves read NOW, not from
                            // the last poll: on a dumping curve a stale quote
                            // sets a slippage floor the pool can no longer
                            // pay, and the simulation rejects every retry
                            // with the same custom program error. The buy
                            // path has refreshed before quoting for ages —
                            // the sell deserved the same.
                            if let Some(c) = bot.coin.as_mut() {
                                let _ = tokio::time::timeout(
                                    Duration::from_millis(900),
                                    engine::refresh_venue(&bot.rpc, c),
                                )
                                .await;
                            }
                            let est = bot.coin.as_ref().map(|c| c.sol_out(tokens)).unwrap_or(0.0);
                            bot.note(if all {
                                format!("sending SELL ALL (~{est:.6} SOL)…")
                            } else {
                                format!("sending sell {:.0}% (~{est:.6} SOL)…", frac * 100.0)
                            });
                            let sent = {
                                let coin = bot.coin.as_ref().expect("checked above");
                                engine::sell(&bot.rpc, &bot.signer, coin, tokens, slip, cu).await
                            };
                            match sent {
                                Ok(sig) => {
                                    bot.push_order("SELL", est, tokens, OrderState::Pending, Some(sig.clone()));
                                    bot.note(format!("Sell for about {est:.6} SOL sent, signature {sig}"));
                                }
                                Err(e) => {
                                    bot.push_order("SELL", est, tokens, OrderState::Failed, None);
                                    bot.fails += 1;
                                    bot.note(format!("Sell failed. {e}"));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Absorb the poller's latest snapshot — memory only, never blocks.
        let latest = snap.lock().map(|s| s.clone()).unwrap_or_default();
        bot.absorb(&latest);

        // Adopt a coin whose background resolution just landed.
        let landed = pending_coin.lock().unwrap().take();
        if let Some(p) = landed {
            match p.coin {
                Ok(c) => {
                    let tgt = poll_target(&c, &bot.trader(), bot.priority_auto.then(|| bot.priority_level.key()));
                    let graduated = c.graduated();
                    bot.meta = p.meta;
                    // The artwork, in the background. It is decoration: it must
                    // never be a reason the numbers arrive later.
                    if bot.meta.as_ref().and_then(|m| m.image.as_deref()).is_none() {
                        super::trace("art: this coin's metadata names no image");
                    }
                    if let Some(url) = bot.meta.as_ref().and_then(|m| m.image.clone()) {
                        super::trace(&format!("art: fetching {url}"));
                        tokio::spawn(async move {
                            let _ = crate::art::png(&url).await;
                        });
                    }
                    bot.remember_coin(&c);
                    save_last_coin(&p.mint);
                    bot.coin = Some(c);
                    bot.launched_at = p.launched;
                    bot.token_bal = 0.0;
                    bot.bought_qty = 0.0;
                    bot.bought_cost = 0.0;
                    bot.tape.clear();
                    // History first: the trades you watched — and made — last
                    // session are on disk, and the live feed dedups on top.
                    discover::merge_tape(&mut bot.tape, load_tape_history(&p.mint));
                    discover::merge_tape(&mut bot.tape, p.seed);
                    // Sealed candles from past sessions, then bring them
                    // current with whatever the tape already holds.
                    bot.hist = load_candles(&p.mint);
                    bot.reseal_candles();
                    bot.backfill_orders();
                    *target.lock().unwrap() = Some(tgt);
                    view = Panel::Tape;
                    scroll = 0;
                    // Warm the risk cache in the background, as always.
                    let (rk, m2) = (bot.risk.clone(), p.mint.to_string());
                    tokio::spawn(async move { let _ = rk.report(&m2).await; });
                    bot.note(if graduated {
                        format!("Loaded {}, trading on the Pump AMM", p.mint)
                    } else {
                        format!("Loaded {}, trading on the bonding curve", p.mint)
                    });
                }
                Err(e) => bot.note(format!("Could not load {}. {e}", p.mint)),
            }
        }

        // A coin that fills its curve MIGRATES to the AMM. The venue was only
        // resolved when the coin was selected, so a coin that graduated while
        // being watched kept polling its dead curve: the tape went silent and a
        // buy would have been sent to a curve that rejects it. Re-resolve once
        // `complete` flips.
        if bot.needs_migration() && bot.migrate_due() {
            bot.migrate_at = Some(Instant::now());
            let mint = bot.coin.as_ref().map(|c| c.mint);
            if let Some(mint) = mint {
                match engine::load_coin(&bot.rpc, &mint).await {
                    Ok(c) if c.on_amm() => {
                        let tgt = poll_target(&c, &bot.trader(), bot.priority_auto.then(|| bot.priority_level.key()));
                        bot.remember_coin(&c);
                                            bot.coin = Some(c);
                        *target.lock().unwrap() = Some(tgt);
                        bot.note("This coin graduated, so trading moved to the Pump AMM");
                    }
                    // The pool can lag the curve completing by a few seconds.
                    Ok(_) => super::trace("graduated but AMM pool not live yet; will retry"),
                    Err(e) => super::trace(&format!("graduation migrate failed: {e}")),
                }
            }
        }

        // Settling orders is cheap and only runs while something is pending.
        if bot.orders.iter().any(|o| o.state == OrderState::Pending)
            && last_reap.elapsed() >= Duration::from_millis(800)
        {
            bot.reap().await;
            last_reap = Instant::now();
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    poll_handle.abort();
    Ok(exit)
}

#[cfg(test)]
mod priority_tests {
    use super::*;

    /// The cap is the whole safety story for auto mode: the estimate's top rung
    /// was 4.6e10 micro-lamports/CU when this was written, which is over 9 SOL
    /// for a single trade. Whatever an endpoint returns, one trade cannot
    /// exceed PRIORITY_CAP_SOL.
    #[test]
    fn auto_priority_is_capped_however_wild_the_estimate() {
        let cu = super::super::tx::CU_LIMIT_AMM;
        let ceiling = (SolBot::PRIORITY_CAP_SOL * super::super::LAMPORTS_PER_SOL as f64) as u128
            * 1_000_000
            / cu as u128;

        // An absurd answer clamps to exactly the cap.
        let clamped = (46_029_829_508u128).min(ceiling) as u64;
        assert_eq!(clamped as u128, ceiling);
        let cost = super::super::tx::priority_fee_sol(clamped, cu);
        assert!(cost <= SolBot::PRIORITY_CAP_SOL + 1e-12, "{cost} SOL exceeds the cap");

        // A sane answer passes through untouched.
        let sane = 240_085u64;
        assert_eq!((sane as u128).min(ceiling) as u64, sane);
        assert!(super::super::tx::priority_fee_sol(sane, cu) < SolBot::PRIORITY_CAP_SOL);
    }

    /// Holding the key down must not wrap from the most aggressive rung back to
    /// the cheapest one mid-launch.
    #[test]
    fn priority_levels_clamp_rather_than_wrap() {
        assert_eq!(PriorityLevel::Medium.step(false), PriorityLevel::Medium);
        assert_eq!(PriorityLevel::VeryHigh.step(true), PriorityLevel::VeryHigh);
        assert_eq!(PriorityLevel::Medium.step(true), PriorityLevel::High);
        assert_eq!(PriorityLevel::High.step(true), PriorityLevel::VeryHigh);
        assert_eq!(PriorityLevel::VeryHigh.step(false), PriorityLevel::High);
        // The RPC keys must match what the endpoint expects, exactly.
        assert_eq!(PriorityLevel::VeryHigh.key(), "veryHigh");
        assert_eq!(PriorityLevel::High.key(), "high");
    }

    use super::super::tx;

    /// Halving with integer division reaches 0, and doubling 0 stays 0 — the
    /// setting would be stuck off with no key able to raise it again.
    #[test]
    fn priority_never_gets_stuck_at_zero() {
        let mut cu: u64 = 1_000;
        for _ in 0..20 {
            cu = (cu / 2).max(1);
        }
        assert_eq!(cu, 1, "halving must floor at 1, not reach 0");
        cu = (cu.max(1) * 2).min(10_000_000);
        assert_eq!(cu, 2, "doubling from the floor must actually raise it");
    }

    /// The reserve has to cover what a buy really costs — and no more. A flat
    /// 0.01 SOL was roughly four times the true figure, and since it lands in a
    /// `min` it acts as a floor: a wallet holding less than the reserve sized
    /// EVERY buy to zero, at any percentage, reporting only "buy size must be
    /// > 0". A wallet with a few thousandths of a SOL must be able to trade.
    #[test]
    fn the_buy_reserve_covers_the_real_cost_without_freezing_small_wallets() {
        let priority = tx::priority_fee_sol(240_085, tx::CU_LIMIT_AMM);
        let reserve = super::reserve_for_buy(priority);

        // Above the unavoidable floor: token-account rent plus the signature.
        let unavoidable =
            super::super::lamports_to_sol(tx::SIGNATURE_FEE_LAMPORTS + tx::ATA_RENT_LAMPORTS);
        assert!(reserve > unavoidable, "{reserve} would not cover rent + fee ({unavoidable})");
        assert!(reserve > unavoidable + priority, "the priority fee must be reserved too");
        // Enough left over to SELL: an entry you cannot exit is the worse bug.
        let exit = super::super::lamports_to_sol(tx::SIGNATURE_FEE_LAMPORTS) + priority;
        assert!(
            reserve > unavoidable + priority + exit,
            "{reserve} leaves nothing to pay for the sell"
        );

        // ...and well under the old flat figure, which is the whole point.
        assert!(reserve < 0.01, "{reserve} is no better than the flat 0.01 it replaced");

        // The wallet that could not buy at all: ~0.005 SOL, every size zero.
        let sol = 0.005;
        assert!(
            (sol - 0.01f64).max(0.0) == 0.0,
            "the old reserve zeroed this wallet — that is the bug being fixed"
        );
        let sized = (sol * 0.05).min((sol - reserve).max(0.0));
        assert!(sized > 0.0, "a 0.005 SOL wallet must be able to place a 5% buy");
    }

    /// One setting must cost more on the venue that uses more compute, and the
    /// figure shown has to match whichever venue the next trade will use.
    #[test]
    fn an_amm_trade_costs_more_than_a_curve_trade() {
        let curve = tx::priority_fee_sol(10_000, tx::CU_LIMIT_TRADE);
        let amm = tx::priority_fee_sol(10_000, tx::CU_LIMIT_AMM);
        assert!(amm > curve, "the AMM path touches more accounts, so it costs more");
    }
}
