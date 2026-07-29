// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! The Solana dashboard — the pump.fun counterpart to the EVM trading screen.
//!
//! Deliberately mirrors the EVM layout field-for-field: same header (account /
//! slot / round-trip latency / sizing knobs), same Wallet + Market columns, and
//! the same Orders · Tape · Logs cycle on `l`. Renders entirely through the
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
pub struct SeenCoin {
    pub mint: Pubkey,
    /// "SYMBOL · bonding curve" — the venue is part of the identity, since the
    /// same coin reads completely differently once it graduates.
    pub label: String,
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
    /// How many signatures to ask for next round.
    depth: u32,
}

impl TapeCache {
    /// Point the cache at a coin, dropping another coin's history.
    fn retarget(&mut self, mint: Option<Pubkey>) {
        if self.mint != mint {
            self.mint = mint;
            self.rows.clear();
            self.seen.clear();
            self.depth = TAPE_DEPTH;
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
async fn poller(
    rpc: Rpc,
    trader: Pubkey,
    target: Arc<Mutex<Option<PollTarget>>>,
    out: Arc<Mutex<Snapshot>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let http = reqwest::Client::new();
    let mut sol_usd = 0.0f64;
    let mut tape_cache = TapeCache::default();
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let t0 = Instant::now();
        let tgt = *target.lock().unwrap();
        tape_cache.retarget(tgt.as_ref().map(|t| t.mint));

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
                    let (bals, batch) = tokio::join!(
                        rpc.token_balances(&want),
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
                        ),
                    );
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
                        discover::pool_tape(&rpc, &t.mint, tape_cache.depth(), &trader, &tape_cache.seen),
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
        tokio::time::sleep(REFRESH).await;
    }
}

/// Which panel occupies the lower half — mirrors the EVM `Panel`.
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Orders,
    Tape,
    Logs,
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
    pub buy_sol: f64,
    pub slippage_pct: f64,
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
}

impl SolBot {
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
            buy_sol: 0.01,
            slippage_pct: 5.0,
            sell_frac: 1.00,
            cu_price_micro: 10_000,
            coins: Vec::new(),
            priority_auto: false,
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
            launched_at: None,
            sol_usd: 0.0,
            risk: super::rugcheck::RugCheck::new(rc),
            warn_score: rc.warn_score,
            http: reqwest::Client::new(),
            status: "Press f to find a coin".into(),
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
    pub fn live_edge(&self) -> f64 {
        match &self.coin {
            Some(c) if self.token_bal > 0.0 => c.sol_out(self.token_bal) - self.bought_cost,
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

    fn push_order(&mut self, action: &'static str, sol: f64, state: OrderState, sig: Option<String>) {
        let (mc, pooled) = self
            .coin
            .as_ref()
            .map(|c| (c.market_cap_sol(), c.pooled_sol()))
            .unwrap_or((0.0, 0.0));
        self.orders.push_back(SolOrder {
            at: std::time::Instant::now(),
            action,
            sol,
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
        for (i, sig) in pending {
            let Ok(Some(ok)) = self.rpc.signature_ok(&sig).await else { continue };
            let (action, sol) = match self.orders.get(i) {
                Some(o) => (o.action, o.sol),
                None => continue,
            };
            if let Some(o) = self.orders.get_mut(i) {
                o.state = if ok { OrderState::Confirmed } else { OrderState::Failed };
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
                let pnl = sol - self.bought_cost;
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
                        cost: self.bought_cost,
                        proceeds: sol,
                        quote_sym: "SOL".into(),
                        quote_usd: self.sol_usd,
                        tx: sig.clone(),
                        held_secs: self.entry_at.map(|t| crate::ledger::now().saturating_sub(t)),
                    },
                );
                self.bought_cost = 0.0;
                self.bought_qty = 0.0;
                self.entry_at = None;
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
    p.spans(vec![lbl("Account"), Cell::toned(bot.trader().to_string(), Tone::Info)]);
    p.spans(vec![lbl("SOL"), Cell::new(format!("{:.6}", bot.sol))]);
    p.spans(vec![lbl("Token"), Cell::new(format!("{:.4}", bot.token_bal))]);
    // Name the unit: "SOL/tok" is ambiguous once several coins are in play.
    let unit = bot.meta.as_ref().map(|m| m.symbol.as_str()).unwrap_or("tok");
    p.spans(vec![lbl("Basis"), Cell::new(format!("{:.9} SOL/{unit}", bot.avg_basis()))]);
    p.spans(vec![lbl("Inventory"), Cell::new(format!("{:.2} (bought)", bot.bought_qty))]);
    p.spans(vec![
        lbl("Realized"),
        Cell::bold(format!("{:+.6} SOL", bot.realized_pnl), pnl_tone(bot.realized_pnl)),
    ]);
    p.spans(vec![
        lbl("Last Fill"),
        match bot.last_fill_pnl {
            Some(v) => Cell::bold(format!("{v:+.6} SOL"), pnl_tone(v)),
            None => Cell::bold("—", Tone::Dim),
        },
    ]);
    let edge = bot.live_edge();
    p.spans(vec![lbl("Edge"), Cell::bold(format!("{edge:+.7} SOL"), pnl_tone(edge))]);
    p.spans(vec![
        lbl("Activity"),
        Cell::new(format!(
            "{} trades  {} fails  {} pending",
            bot.trades,
            bot.fails,
            bot.orders.iter().filter(|o| o.state == OrderState::Pending).count()
        )),
    ]);
    p.spans(vec![lbl("Buy Size"), Cell::toned(format!("{:.4} SOL", bot.buy_sol), Tone::Accent)]);
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
            p.line_toned("  [t] top coins", Tone::Info);
            p.line_toned("  [p] add a coin by mint address", Tone::Info);
        }
        Some(c) => {
            p.spans(vec![
                lbl("Venue"),
                Cell::bold(
                    if c.on_amm() { "PumpSwap AMM (graduated)" } else { "bonding curve" },
                    if c.on_amm() { Tone::Info } else { Tone::Accent },
                ),
            ]);
            if let Some(m) = &bot.meta {
                p.spans(vec![
                    lbl("Token"),
                    Cell::bold(format!("{} ({})", m.name, m.symbol), Tone::Accent),
                ]);
            }
            p.spans(vec![lbl("Mint"), Cell::new(c.mint.to_string())]);
            // The pool is what you actually trade against, and it's the address
            // every chart and explorer keys off — worth showing next to the mint.
            if let Some(pair) = c.pair_address() {
                p.spans(vec![lbl("Pair"), Cell::new(pair.to_string())]);
            }
            match bot.risk.cached(&c.mint.to_string()) {
                Some(rep) => {
                    p.spans(vec![lbl("Risk"), Cell::bold(rep.summary(), rep.tone(bot.warn_score))]);
                    if rep.lp_locked_pct > 0.0 {
                        p.spans(vec![lbl("LP Locked"), Cell::new(format!("{:.0}%", rep.lp_locked_pct))]);
                    }
                }
                None => p.spans(vec![lbl("Risk"), Cell::toned("checking…", Tone::Dim)]),
            }
            p.spans(vec![lbl("Price"), Cell::new(format!("{:.9} SOL", c.price_sol()))]);
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
                format!("  (~{})", crate::view::usd_compact(pooled * bot.sol_usd))
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
            " Orders {}–{} of {} ↑/↓ scroll  ·  {}  [l] ",
            scroll + 1,
            (scroll + h).min(total),
            total,
            bot.trader()
        )
    } else {
        // The trader is the same wallet on every row, so it belongs in the
        // title once rather than eating 44 columns per line.
        format!(" Orders ({total})  [l] ")
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
            Col::fixed("entry mc", 11),
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
        let atone = if o.action == "BUY" { Tone::Good } else { Tone::Bad };
        t.push(vec![
            Cell::new(crate::view::age_compact(o.at.elapsed().as_secs_f64())),
            Cell::bold(st, tone),
            Cell::bold(o.action, atone),
            Cell::new(format!("{:.6}", o.sol)),
            Cell::new(if o.mc > 0.0 { format!("{:.2}", o.mc) } else { String::new() }),
            Cell::new(if o.pooled > 0.0 { format!("{:.3}", o.pooled) } else { String::new() }),
            Cell::toned(o.sig.clone().unwrap_or_default(), Tone::Normal),
        ]);
    }
    t
}

fn logs_panel(bot: &SolBot, scroll: usize, h: usize) -> PanelView {
    let mut p = PanelView::new(" Logs [l] ");

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

const HELP: [(&str, &str); 22] = [
    ("TRADE", ""),
    ("", "b|buy"),
    ("", "s|sell a slice of the balance"),
    ("", "x|sell the whole balance"),
    ("DISCOVER", ""),
    ("", "f|find market (live launches)"),
    ("", "p|add token by contract address"),
    ("VIEW", ""),
    ("", "l  → ←|cycle orders · trades · logs"),
    ("", "↑ ↓|scroll"),
    ("SIZE", ""),
    ("", "[  ]|buy size −/+"),
    ("", "(  )|slippage −/+"),
    ("", "{  }|priority fee −/+"),
    ("", "P|auto priority on/off"),
    ("MODE", ""),
    ("", "T|theme picker"),
    ("", "C|change chain"),
    ("", "W|change wallet"),
    ("", "D|docs"),
    ("", "q|quit (Q skips the prompt)"),
    ("", "?|help"),
];

/// Draws the dashboard and returns where the header logo goes, so the caller
/// can place a real terminal image there after the frame.
fn draw(f: &mut Frame, bot: &SolBot, view: Panel, scroll: usize, show_help: bool) -> Option<Rect> {
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
            Style::default().fg(ui::widgets::tone_color(Tone::Info)).add_modifier(Modifier::BOLD),
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

    let head_block = ui::widgets::themed_block(" Trenches Bot [C] ");
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
                val(format!("{:<14}", format!("{:.4} SOL", bot.buy_sol))),
                hint("[M] "),
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
                val(bot.status.clone()),
            ]),
        ])
        .block(ui::widgets::themed_block(" Settings ")),
        c[2],
    );

    let h = c[3].height.saturating_sub(3).max(1) as usize;
    match view {
        Panel::Orders => ui::widgets::table(f, c[3], &orders_table(bot, scroll, h), None),
        Panel::Tape => ui::widgets::table(f, c[3], &discover::tape_view(&bot.tape, scroll, h, bot.sol_usd), None),
        Panel::Logs => ui::widgets::panel(f, c[3], &logs_panel(bot, scroll, h + 1)),
    }

    let key = |k: &'static str, t: Tone| {
        Span::styled(k, Style::default().fg(ui::widgets::tone_color(t)).add_modifier(Modifier::BOLD))
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
        key("[l]", Tone::Normal),
        Span::raw(" logs  "),
        key("[T]", Tone::Info),
        Span::raw(" theme  "),
        key("[?]", Tone::Info),
        Span::raw(" help  "),
        key("[C]", Tone::Info),
        Span::raw(" chain  "),
        key("[W]", Tone::Info),
        Span::raw(" wallet  "),
        key("[D]", Tone::Info),
        Span::raw(" docs  "),
        key("[q]", Tone::Info),
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
            Style::default().fg(ui::widgets::tone_color(Tone::Good)).add_modifier(Modifier::BOLD),
        ));
        keys.push(Span::styled(
            format!(" update to {v}"),
            Style::default().fg(ui::widgets::tone_color(Tone::Good)),
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
        ui::widgets::help(f, &HELP, " Shortcuts  (any key to close) ");
    }
    Some(logo_box)
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

async fn screen_trenches(
    term: &mut Term,
    rpc: &Rpc,
    ws_urls: &[String],
    sol_usd: f64,
    risk: &super::rugcheck::RugCheck,
    warn_score: u32,
) -> eyre::Result<Option<(Pubkey, Option<i64>)>> {
    // This screen owns the terminal now: take down the dashboard's image, which
    // also marks it stale so it redraws when we come back.
    ui::image::clear();
    let found: Arc<Mutex<Vec<TrenchRow>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // What the feed is doing right now. Shown in the empty state so a stalled
    // websocket reads as a stalled websocket instead of as a quiet market.
    let feed: Arc<Mutex<String>> = Arc::new(Mutex::new("connecting to the launch feed…".to_string()));

    let (f2, s2, rpc2, ws2, fe2) =
        (found.clone(), stop.clone(), rpc.clone(), ws_urls.to_vec(), feed.clone());
    let handle = tokio::spawn(async move {
        let fe3 = fe2.clone();
        let r = discover::watch_launches_ha(
            &rpc2,
            &ws2,
            |launch| {
                let (f3, rpc3) = (f2.clone(), rpc2.clone());
                super::trace(&format!("launch {}", launch.mint));
                tokio::spawn(async move {
                    let rows = discover::enrich(&rpc3, vec![launch]).await;
                    super::trace(&format!("enrich -> {} row(s)", rows.len()));
                    for row in rows {
                        f3.lock().unwrap().push(row);
                    }
                });
                !s2.load(std::sync::atomic::Ordering::Relaxed)
            },
            move |msg| {
                super::trace(&format!("feed: {msg}"));
                if let Ok(mut f) = fe3.lock() {
                    *f = msg;
                }
            },
        )
        .await;
        // The old code discarded this. A failed websocket then looked exactly
        // like an idle one — the screen said "watching…" forever with no hint.
        if let Err(e) = r {
            super::trace(&format!("feed DOWN: {e}"));
            if let Ok(mut f) = fe2.lock() {
                *f = format!("launch feed down: {e}");
            }
        }
    });

    // Keep the visible rows live. Without this a coin discovered at 20% bonded
    // showed 20% forever while it actually filled — the one number that says
    // "this launch is working" was frozen at discovery.
    let (f4, s4, rpc4) = (found.clone(), stop.clone(), rpc.clone());
    let refresher = tokio::spawn(async move {
        while !s4.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let mints: Vec<Pubkey> = {
                let rows = f4.lock().unwrap();
                rows.iter().map(|r| r.launch.mint).collect()
            };
            if mints.is_empty() {
                continue;
            }
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
        }
    });

    let mut cursor = ui::widgets::Cursor::new();
    let result = loop {
        let mut rows = found.lock().unwrap().clone();
        discover::sort_newest_first(&mut rows);
        // Warm one uncached report per pass: keeps the list filling in without
        // hammering the free tier or stalling the loop.
        if let Some(r) = rows.iter().find(|r| risk.cached(&r.launch.mint.to_string()).is_none()) {
            let _ = risk.report(&r.launch.mint.to_string()).await;
        }
        let mut table = discover::table_view(&rows, sol_usd, Some(risk), warn_score);
        // Replace the generic note with what the feed is actually doing.
        if rows.is_empty() {
            let status = feed.lock().map(|f| f.clone()).unwrap_or_default();
            table.empty_note = format!("{status}\nnew launches appear the moment they are created  ·  esc to go back");
        }
        let n = rows.len();
        term.draw(|f| {
            ui::widgets::paint_bg(f);
            let st = cursor.state_for(n);
            ui::widgets::table(f, f.area(), &table, Some(st));
        })?;

        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(k) = event::read()? {
                match cursor.on_key(k.code, n) {
                    ui::widgets::Nav::Enter => break rows.get(cursor.sel).map(|r| (r.launch.mint, r.launch.block_time)),
                    ui::widgets::Nav::Back => break None,
                    _ => {}
                }
            }
        }
    };

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    handle.abort();
    refresher.abort();
    Ok(result)
}

// ---- main loop -----------------------------------------------------------

pub async fn run(
    term: &mut Term,
    rpc_urls: Vec<String>,
    ws_override: Option<&str>,
    signer: Keypair,
    // False when nobody unlocked a keystore: the signer is a throwaway and must
    // never be asked to sign. Reads work; the order keys do not.
    has_account: bool,
    rugcheck: &crate::config::RugCheck,
    net: &str,
) -> eyre::Result<crate::Exit> {
    let rpc = Rpc::new_pool(rpc_urls.clone());
    // Providers frequently host WS on a separate domain, so an explicit setting
    // wins over deriving it from the HTTP URL.
    // Candidates in preference order: the explicit setting first, then one
    // derived from each RPC endpoint. Providers host WS on a separate domain
    // often enough that deriving alone isn't reliable, but having the derived
    // ones as fallbacks means a single provider outage can't blind the feed.
    let mut ws_urls: Vec<String> = Vec::new();
    if let Some(w) = ws_override {
        ws_urls.push(w.to_string());
    }
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
    let mut show_help = false;
    let mut view = Panel::Orders;
    let mut scroll: usize = 0;
    let mut last_reap = Instant::now();
    // Force an immediate first refresh.
    // Background poller owns all timed RPC; the UI thread only reads snapshots.
    let snap: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(Snapshot::default()));
    let target: Arc<Mutex<Option<PollTarget>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poll_handle = tokio::spawn(poller(
        bot.rpc.clone(),
        bot.trader(),
        target.clone(),
        snap.clone(),
        stop.clone(),
    ));

    loop {
        let mut logo_box = None;
        term.draw(|f| {
            logo_box = draw(f, &bot, view, scroll, show_help);
        })?;
        let venue = header_venue(&bot);
        let term_size = term.size().map(|s| (s.width, s.height)).unwrap_or((0, 0));
        if let (Some(r), Some(png)) = (logo_box, ui::image::for_venue(venue, &bot.net)) {
            chain_logo.show(png, venue as usize, r.x, r.y, r.width, r.height, term_size);
        }

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
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
                        crate::update::Status::Latest => bot.note(format!(
                            "You are on the latest version ({}).",
                            crate::update::full()
                        )),
                        crate::update::Status::Update(v) => {
                            if ui::confirm(term, &format!("Update to {v}?"))? {
                                crate::events::action("Updating", &[("to", v.clone())]);
                                bot.note(format!("Installing {v}…"));
                                match crate::update::install_latest() {
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
                    KeyCode::Char('W') => {
                        exit = crate::Exit::ChangeAccount;
                        break;
                    }
                    // Back to the chain picker without restarting the binary.
                    KeyCode::Char('C') => {
                        exit = crate::Exit::ChangeChain;
                        break;
                    }
                    KeyCode::Char('?') => show_help = true,
                    KeyCode::Char('T') => match ui::widgets::theme_picker(term)? {
                        Some(name) => bot.note(format!("Changed theme to {name}")),
                        None => bot.note("theme unchanged"),
                    },
                    KeyCode::Char('l') | KeyCode::Right => {
                        view = match view {
                            Panel::Orders => Panel::Tape,
                            Panel::Tape => Panel::Logs,
                            Panel::Logs => Panel::Orders,
                        };
                        scroll = 0;
                    }
                    KeyCode::Left => {
                        view = match view {
                            Panel::Orders => Panel::Logs,
                            Panel::Logs => Panel::Tape,
                            Panel::Tape => Panel::Orders,
                        };
                        scroll = 0;
                    }
                    KeyCode::Up => {
                        let n = match view {
                            Panel::Logs => bot.logs.len(),
                            Panel::Tape => bot.tape.len(),
                            Panel::Orders => bot.orders.len(),
                        };
                        scroll = (scroll + 1).min(n.saturating_sub(1));
                    }
                    KeyCode::Down => scroll = scroll.saturating_sub(1),
                    KeyCode::Char(']') => bot.buy_sol = (bot.buy_sol * 1.5).min(100.0),
                    KeyCode::Char('[') => bot.buy_sol = (bot.buy_sol / 1.5).max(0.0001),
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
                                    bot.note(format!("loading {mint}…"));
                                    match engine::load_coin(&bot.rpc, &mint).await {
                                        Ok(c) => {
                                            let tgt = poll_target(&c, &bot.trader(), bot.priority_auto.then(|| bot.priority_level.key()));
                                            let graduated = c.graduated();
                                            bot.meta = super::metadata::token_meta(&bot.rpc, &mint).await;
                                            bot.remember_coin(&c);
                                            bot.coin = Some(c);
                                            bot.launched_at = None;
                                            bot.token_bal = 0.0;
                                            bot.bought_qty = 0.0;
                                            bot.bought_cost = 0.0;
                                            bot.tape.clear();
                                            *target.lock().unwrap() = Some(tgt);
                                            view = Panel::Tape;
                                            scroll = 0;
                                            let _ = bot.risk.report(&mint.to_string()).await;
                                            bot.note(if graduated {
                                                format!("Loaded {mint}, trading on the Pump AMM")
                                            } else {
                                                format!("Loaded {mint}, trading on the bonding curve")
                                            });
                                        }
                                        Err(e) => bot.note(format!("Could not load that coin. {e}")),
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Char('f') => {
                        bot.note("Watching for new launches");
                        if let Some((mint, launched)) = screen_trenches(term, &bot.rpc, &ws_urls, bot.sol_usd, &bot.risk, bot.warn_score).await? {
                            match engine::load_coin(&bot.rpc, &mint).await {
                                Ok(c) => {
                                    let tgt = poll_target(&c, &bot.trader(), bot.priority_auto.then(|| bot.priority_level.key()));
                                    bot.meta = super::metadata::token_meta(&bot.rpc, &mint).await;
                                    bot.remember_coin(&c);
                                            bot.coin = Some(c);
                                    bot.launched_at = launched;
                                    // New coin -> fresh cost basis, as on the EVM side.
                                    bot.token_bal = 0.0;
                                    bot.bought_qty = 0.0;
                                    bot.bought_cost = 0.0;
                                    bot.tape.clear();
                                    // Land on the Tape: after picking a coin the
                                    // first thing you want is its live flow.
                                    view = Panel::Tape;
                                    scroll = 0;
                                    // Hand the poller the new target; it fills in
                                    // curve/balance/tape on its own thread.
                                    *target.lock().unwrap() = Some(tgt);
                                    let _ = bot.risk.report(&mint.to_string()).await;
                                    bot.note(format!("Loaded {mint}"));
                                }
                                Err(e) => bot.note(format!("Could not load that coin. {e}")),
                            }
                        } else {
                            bot.note("cancelled");
                        }
                    }
                    KeyCode::Char('b') => {
                        let (sol, slip, cu) = (bot.buy_sol, bot.slippage_pct, bot.cu_price_micro);
                        if bot.coin.is_none() {
                            bot.note("No coin is selected");
                        } else if sol > bot.sol {
                            // Refuse locally rather than burning a fee on a
                            // transaction the node will reject anyway.
                            bot.note(format!("Not enough SOL. You have {:.6} and need {sol:.6}", bot.sol));
                        } else {
                            bot.note(format!("Sending a buy for {sol:.6} SOL"));
                            let sent = {
                                let coin = bot.coin.as_ref().expect("checked above");
                                engine::buy(&bot.rpc, &bot.signer, coin, sol, slip, cu).await
                            };
                            match sent {
                                Ok(sig) => {
                                    bot.push_order("BUY", sol, OrderState::Pending, Some(sig.clone()));
                                    bot.note(format!("Buy for {sol:.6} SOL sent, signature {sig}"));
                                }
                                Err(e) => {
                                    bot.push_order("BUY", sol, OrderState::Failed, None);
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
                        let (tokens, slip, cu) =
                            (bot.token_bal * frac, bot.slippage_pct, bot.cu_price_micro);
                        if bot.coin.is_none() {
                            bot.note("No coin is selected");
                        } else if tokens <= 0.0 {
                            bot.note("There is nothing to sell");
                        } else {
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
                                    bot.push_order("SELL", est, OrderState::Pending, Some(sig.clone()));
                                    bot.note(format!("Sell for about {est:.6} SOL sent, signature {sig}"));
                                }
                                Err(e) => {
                                    bot.push_order("SELL", est, OrderState::Failed, None);
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

    /// One setting must cost more on the venue that uses more compute, and the
    /// figure shown has to match whichever venue the next trade will use.
    #[test]
    fn an_amm_trade_costs_more_than_a_curve_trade() {
        let curve = tx::priority_fee_sol(10_000, tx::CU_LIMIT_TRADE);
        let amm = tx::priority_fee_sol(10_000, tx::CU_LIMIT_AMM);
        assert!(amm > curve, "the AMM path touches more accounts, so it costs more");
    }
}
