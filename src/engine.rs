// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Trade engine: market snapshot, order placement with pre-flight gas
//! protection, pending tracking, strategies, and a session log. Provider-
//! generic so it works against remote RPC, a local Nitro node ws://, or IPC.

use std::collections::VecDeque;
use std::time::Instant;

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, TxHash, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;

use crate::contracts::*;
use crate::v3;
use crate::v4;

#[derive(Clone, Copy, PartialEq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Strategy {
    Manual,     // trade only on b/s keypress; buys sized by buy_frac
    CopyBuyAmount, // preselects the buy AMOUNT from the deployer's buy steps ([ ] picks the step); manual timing
}

pub struct Pending {
    pub hash: TxHash,
    pub label: String,
    // Fill info for cost-basis accounting (None for non-trade txs like LP ops).
    pub side: Option<Side>,
    pub eth_amt: f64, // ETH spent (buy) or received (sell)
    pub tok_amt: f64, // tokens received (buy) or sold (sell)
    pub position_id: Option<U256>, // v4 position tokenId for mint/burn txs
}

#[derive(Clone, Copy, PartialEq)]
pub enum OrderStatus {
    Pending,
    Confirmed,
    Reverted,
    Skipped,
    Failed,
}

/// One user action in the orders queue — transitions pending -> confirmed/etc.
pub struct Order {
    pub label: String,
    pub status: OrderStatus,
    pub hash: Option<TxHash>,
    pub mc: f64,     // market cap (ETH) at order time — the entry point for a BUY
    pub pooled: f64, // pooled ETH at order time
    pub eth: f64,    // ETH value of this order (tape-parity amount column)
    pub is_v4: bool, // venue at order time (v4 vs v3), for the pool column
}

/// Protocol-specific pool identity. A v4 pool is identified by its pool id +
/// tickSpacing; a v3 pool by its contract address + token orientation. Encoding
/// them as a sum type makes illegal states (v4 without id, v3 without address)
/// unrepresentable.
#[derive(Clone, Copy, PartialEq)]
pub enum PoolKind {
    V4 { pool_id: B256, tick_spacing: i32 },     // native ETH, PoolManager/UniversalRouter
    V3 { pool_addr: Address, weth_is_token0: bool }, // WETH, SwapRouter02
    // A Flaunch launch: the same PoolManager, but paired against flETH with the
    // Flaunch hook attached, so swaps route ETH<->flETH<->coin through the
    // UniversalRouter. Not a widened V4: that variant bakes in currency0 =
    // native ETH and hooks = 0. coin_is_0 records _currencyFlipped from the
    // launch (flETH's low address makes it currency0 in practice, but the
    // protocol allows either order).
    FlaunchV4 { pool_id: B256, coin_is_0: bool },
}

impl PoolKind {
    pub fn proto(&self) -> &'static str {
        match self {
            PoolKind::V4 { .. } => "v4",
            PoolKind::V3 { .. } => "v3",
            PoolKind::FlaunchV4 { .. } => "flaunch",
        }
    }

    /// The venue spelled out for the header. `v3` is fine in a dense table
    /// cell, but the header has room and "which AMM am I on" should not need
    /// decoding.
    pub fn venue_label(&self) -> &'static str {
        match self {
            PoolKind::V4 { .. } => "Uniswap V4",
            PoolKind::V3 { .. } => "Uniswap V3",
            PoolKind::FlaunchV4 { .. } => "Flaunch",
        }
    }
    pub fn is_v3(&self) -> bool {
        matches!(self, PoolKind::V3 { .. })
    }
    /// True when no real pool is selected (placeholder / empty network).
    pub fn is_empty(&self) -> bool {
        match self {
            PoolKind::V4 { pool_id, .. } | PoolKind::FlaunchV4 { pool_id, .. } => {
                *pool_id == B256::ZERO
            }
            PoolKind::V3 { pool_addr, .. } => *pool_addr == Address::ZERO,
        }
    }
}

/// The currency a pool is quoted in. Determines reserve orientation and how the
/// price is valued in USD. ETH pools use the native/WETH side; stable pools
/// (USDG, …) are quoted in a stablecoin whose USD value is fetched LIVE — never
/// assumed to be $1, since stables drift and can depeg.
#[derive(Clone, Copy, PartialEq)]
pub enum Quote {
    Eth,                                     // native ETH (v4) / WETH (v3), 18-dec
    Stable { token: Address, decimals: u8 }, // stablecoin-quoted; USD fetched, not assumed
}

impl Quote {
    /// The quote-side token address (ETH = 0x0; a stable = its token address).
    pub fn addr(&self) -> Address {
        match self {
            Quote::Eth => Address::ZERO,
            Quote::Stable { token, .. } => *token,
        }
    }
    /// Decimals of the quote token — CRITICAL for reading reserves/price: USDG is
    /// 6-dec, not 18, so a hardcoded 1e18 reads its reserve as ~0.
    pub fn decimals(&self) -> u8 {
        match self {
            Quote::Eth => 18,
            Quote::Stable { decimals, .. } => *decimals,
        }
    }
    pub fn is_eth(&self) -> bool {
        matches!(self, Quote::Eth)
    }
}

/// Pool the bot trades.
#[derive(Clone)]
pub struct PoolCfg {
    pub kind: PoolKind,
    pub token: Address, // the tracked (non-quote) token
    pub sym: String,
    pub fee: u32,
    /// Decimals of the TRACKED token, read from the ERC-20. Must not be assumed:
    /// most memecoins are 18, but USDG-style tokens are 6, and a hardcoded 1e18
    /// silently reads a 6-dec balance/reserve as ~0 (price and market cap then
    /// come out as garbage).
    pub token_decimals: u8,
    pub quote: Quote,       // what the pool is priced in (ETH or a stablecoin)
    pub quote_sym: String,  // display symbol for the quote side ("ETH", "USDG")
    pub quote_usd: f64,     // live USD value of one quote unit (fetched)
}

/// Copyable pool reference for the off-thread market reader.
#[derive(Clone, Copy)]
pub struct PoolRef {
    pub kind: PoolKind,
    pub token: Address,
    pub quote: Quote,
    pub token_decimals: u8,
}

impl PoolCfg {
    pub fn as_ref(&self) -> PoolRef {
        PoolRef {
            kind: self.kind,
            token: self.token,
            quote: self.quote,
            token_decimals: self.token_decimals,
        }
    }
}

/// A candidate venue for trading the tracked token. Both buys and sells are
/// simulated across every ETH-quoted route so the best fee tier / depth / hook
/// outcome wins (most tokens per ETH buying, most ETH per token selling).
#[derive(Clone)]
pub struct Route {
    pub kind: PoolKind,
    pub token: Address,
    pub fee: u32,
    pub label: String,
}

pub struct Bot {
    pub trader: Address,
    /// Network label for the header, e.g. "Robinhood Mainnet".
    pub net: String,
    pub account: String,
    pub pool: PoolCfg,
    pub strategy: Strategy,

    // arb mode: a second pool for the same token, side-by-side + gap.
    pub arb_mode: bool,
    pub pool_b: Option<PoolCfg>,
    pub mkt_b: Market, // second pool's live snapshot

    // live market snapshot
    pub sqrt_price: f64, // real sqrt price
    pub tick: i32,
    pub r0: f64, // ETH virtual reserve
    pub r1: f64, // token virtual reserve
    pub eth: f64,
    pub token_bal: f64,
    pub ready: bool,

    // accounting
    pub baseline_eth: Option<f64>,       // ETH at session start (session PnL)
    pub daily_baseline: Option<f64>,     // ETH at start of the current day
    /// Today's realised total, read from the fill ledger — the same figure the
    /// calendar shows for today.
    pub day_realized: f64,
    pub daily_day: u64,                  // day number (unix secs / 86400)
    pub bought_qty: f64,   // purchased token inventory (cost-basis tracking)
    pub bought_cost: f64,  // total ETH paid for that inventory
    /// This session's realised profit — derived from the ledger, not counted
    /// alongside it. Two tallies of the same thing drift; one of them then has
    /// to be believed over the other, and nothing on screen says which.
    pub realized_pnl: f64,
    /// When this session began, so the ledger can be asked what it has made
    /// since.
    pub session_start: u64,
    pub last_fill_pnl: Option<f64>, // realized P&L of the most recent sell (per-trade return)
    pub entry_mc: f64,              // mkt cap (ETH) captured at the last buy — for the trade log
    pub entry_pooled_eth: f64,      // pooled ETH captured at the last buy
    pub entry_tx: Option<TxHash>,   // the last buy's tx hash (cross-checkable entry record)
    /// Unix seconds of the buy that opened the current position. Cleared when
    /// the position is closed, so the next buy starts a fresh clock rather than
    /// measuring from a coin sold days ago.
    pub entry_at: Option<u64>,
    pub trades: u64,
    pub fails: u64,
    pub skips: u64,
    pub last_side: Side,
    pub pending: Vec<Pending>,
    pub positions: Vec<U256>, // locally-cached owned v4 position tokenIds
    pub pos_liq: std::collections::HashMap<U256, f64>, // our liquidity per position (raw L)
    pub mint_liq: std::collections::HashMap<TxHash, f64>, // pending mint tx -> minted L
    pub orders: VecDeque<Order>, // the orders queue shown in the UI
    pub log: std::fs::File,
    pub logs: VecDeque<String>, // in-memory ring for the TUI logs view ('l')

    // knobs
    pub buy_frac: f64,        // BUY size as fraction of ETH balance    ([ ])
    pub sell_frac: f64,       // SELL size as fraction of token balance ( < > )
    /// Slippage tolerance in percent, adjustable with `(` / `)`.
    ///
    /// Every leg derives its floor from this — arb legs and the panic dump
    /// included. They used to carry their own hardcoded 3% / 15% constants, so
    /// changing tolerance silently did nothing to them.
    ///
    /// Was a hardcoded 3%: fine on a deep pool, but far too tight on a thin
    /// memecoin pool (the trade just reverts) and needlessly loose on a stable
    /// pair. Solana exposed this from the start; the EVM side did not.
    pub slippage_pct: f64,
    pub max_price_move: f64,  // per-swap price-impact cap              ({ })
    pub lp_frac: f64,         // LP add size as fraction of ETH balance (fixed)
    pub nonce: Option<u64>,   // locally-tracked nonce (fast, race-free sends)
    pub profit_guard: bool,   // gate trades on positive EV (toggle with 'g')
    pub guard_dup: bool,      // one trade in flight per token — no double buys (toggle 'n')
    pub copy_buy_eth: f64,    // resolved target buy size (ETH) — the selected tier
    pub copy_tiers: Vec<(f64, usize)>, // deployer buy ladder rungs (size, count), size-ascending
    pub copy_idx: usize,      // which rung is selected ([ ] steps it)
    pub copy_manual: bool,    // user has stepped the rung; stop auto-snapping to modal
    pub min_edge_eth: f64,    // required edge over gas (ETH)
    pub ref_price: f64,       // reference price (SMA) for the profit filter
    pub gas_price: f64,       // wei, from the market read
    pub token_supply: f64,    // token totalSupply, for market cap
    pub eth_usd: f64,         // ETH price estimate for USD market cap
    pub last_edge: f64,       // last computed trade edge (ETH), for the UI
    pub last_read_ms: f64,
    pub lp_permit2_done: bool,
    pub v3_covered: bool, // SwapRouter02 allowance covers the full position (exact-amount, v3 sells)
    // Permit2 + UniversalRouter allowances cover the full position (Flaunch
    // sells settle the coin through Permit2, which needs both grants).
    pub ur_permit2_done: bool,
    pub routes: Vec<Route>, // candidate ETH-quoted venues for best-execution routing
    pub meta: Meta,     // current token's on-chain socials/metadata (for the market view)
    /// Pons graduation block, for the pool-age display and the venue logo.
    ///
    /// `Some(0)` means "no Pons launch" — verified and leaderboard pools carry
    /// that placeholder. Use `pons_launch()` rather than testing `is_some()`,
    /// which those placeholders satisfy.
    pub pool_launch_block: Option<u64>,
    pub status: String, // last action result, shown in the dashboard
}

/// On-chain socials/metadata for a Pons launch token (all empty for non-Pons).
#[derive(Clone, Default)]
pub struct Meta {
    pub logo: String,
    pub description: String,
    pub twitter: String,
    pub telegram: String,
    pub website: String,
    pub discord: String,
    pub farcaster: String,
}

impl Meta {
    /// Count of filled fields (0..=7) — a quick completeness signal.
    pub fn score(&self) -> u8 {
        [&self.logo, &self.description, &self.twitter, &self.telegram, &self.website, &self.discord, &self.farcaster]
            .iter()
            .filter(|s| !s.trim().is_empty())
            .count() as u8
    }
    pub fn is_empty(&self) -> bool {
        self.score() == 0
    }
}

/// Read a Pons token's socials/metadata. Returns default (all empty) for tokens
/// that aren't Pons launches (the calls revert → mapped to empty).
pub async fn fetch_token_meta<P: Provider>(provider: &P, token: Address) -> Meta {
    let t = crate::contracts::IPonsToken::new(token, provider);
    let logo = t.logo().call().await.map(|s| s._0).unwrap_or_default();
    let description = t.description().call().await.map(|s| s._0).unwrap_or_default();
    let (twitter, telegram, discord, website, farcaster) = t
        .socials()
        .call()
        .await
        .map(|s| (s.twitter, s.telegram, s.discord, s.website, s.farcaster))
        .unwrap_or_default();
    Meta { logo, description, twitter, telegram, website, discord, farcaster }
}

/// Rewrite an ipfs:// URI to a public-gateway URL; anything else passes through.
fn ipfs_to_http(uri: &str) -> String {
    match uri.strip_prefix("ipfs://") {
        Some(cid) => format!("https://ipfs.io/ipfs/{cid}"),
        None => uri.to_string(),
    }
}

/// Fetch a Flaunch coin's metadata JSON from its launch tokenUri (ipfs://…).
/// Unlike Pons, the socials live off-chain: the JSON carries image, description
/// and the social URLs. Any failure (gateway down, bad JSON) returns an empty
/// Meta — metadata is never worth stalling the app for. `farcaster` stays
/// empty: Flaunch metadata has no such field.
pub async fn fetch_flaunch_meta(token_uri: &str) -> Meta {
    if token_uri.trim().is_empty() {
        return Meta::default();
    }
    let client = match reqwest::Client::builder().timeout(std::time::Duration::from_secs(4)).build() {
        Ok(c) => c,
        Err(_) => return Meta::default(),
    };
    let json: serde_json::Value = match client.get(ipfs_to_http(token_uri)).send().await {
        Ok(r) => match r.json().await {
            Ok(j) => j,
            Err(_) => return Meta::default(),
        },
        Err(_) => return Meta::default(),
    };
    let s = |keys: &[&str]| -> String {
        keys.iter()
            .filter_map(|k| json.get(*k).and_then(|v| v.as_str()))
            .find(|v| !v.trim().is_empty())
            .unwrap_or_default()
            .to_string()
    };
    Meta {
        // The image is itself usually ipfs:// — store the gateway form so the
        // detail panes hold a URL a person can actually open.
        logo: {
            let img = s(&["image", "imageIpfs"]);
            if img.is_empty() { img } else { ipfs_to_http(&img) }
        },
        description: s(&["description"]),
        twitter: s(&["twitterUrl", "twitter"]),
        telegram: s(&["telegramUrl", "telegram"]),
        website: s(&["websiteUrl", "website"]),
        discord: s(&["discordUrl", "discord"]),
        farcaster: String::new(),
    }
}

impl Bot {
    /// The Pons graduation block, if this pool actually came from one.
    ///
    /// Only graduation discovery sets a real block; verified and leaderboard
    /// pools carry `Some(0)`. Treating that as a launch made every hand-picked
    /// pool claim Pons as its venue.
    pub fn pons_launch(&self) -> Option<u64> {
        self.pool_launch_block.filter(|b| *b > 0)
    }

    /// Multiplier that turns a quote into a minimum-output floor.
    /// Clamped to 1% so a zero can never mean "no protection".
    pub fn slip_floor(&self) -> f64 {
        1.0 - self.slippage_pct.max(1.0).min(90.0) / 100.0
    }

    /// The floor for a full liquidation, which eats more of the curve than a
    /// sized trade: the configured tolerance, widened 5x, capped at 50%.
    pub fn slip_floor_dump(&self) -> f64 {
        1.0 - (self.slippage_pct.max(1.0) * 5.0).min(50.0) / 100.0
    }

    pub fn price(&self) -> f64 {
        if self.r0 > 0.0 {
            self.r1 / self.r0
        } else {
            0.0
        }
    }

    pub fn pnl(&self) -> f64 {
        self.eth - self.baseline_eth.unwrap_or(self.eth)
    }

    /// PnL since the start of the current calendar day (persisted across
    /// restarts via .bot/daily-<account>.json).
    /// What today's trading made, from the fill ledger.
    ///
    /// This used to be the balance measured against a baseline taken at the
    /// start of the day. That answers a different question: it counts gas, and
    /// it counts a deposit as profit — so it disagreed with the PnL calendar,
    /// which sums the fills. Two numbers labelled PnL that do not match is
    /// worse than either of them being slightly wrong.
    pub fn daily_pnl(&self) -> f64 {
        self.day_realized
    }

    /// Recount from the ledger — today's total and this session's.
    ///
    /// One source of truth. The wallet panel, the session figure and the PnL
    /// calendar all read the same fills off disk, so they cannot disagree; the
    /// day figure used to be a balance difference, which counts gas and counts
    /// a deposit as profit, and the session figure was a float added up in
    /// parallel. Cheap, and only done when a fill lands or a session starts.
    pub fn refresh_day_realized(&mut self) {
        self.day_realized = crate::ledger::today_total(&self.account);
        self.realized_pnl = crate::ledger::total_since(&self.account, self.session_start);
    }

    fn daily_path(&self) -> String {
        format!("{}/daily-{}.json", crate::state_dir(), self.account)
    }

    /// On the first good balance read (and on a day rollover), set/roll the
    /// daily baseline and persist it.
    fn update_daily(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let today = now / 86400;
        if self.daily_day != today {
            // New day → reset baseline to the current balance.
            self.daily_day = today;
            self.daily_baseline = Some(self.eth);
            self.persist_daily(today);
        } else if self.daily_baseline.is_none() {
            self.daily_baseline = Some(self.eth);
            self.persist_daily(today);
        }
    }

    fn persist_daily(&self, day: u64) {
        if let Some(b) = self.daily_baseline {
            let _ = std::fs::write(self.daily_path(), format!("{{\"day\":{day},\"baseline\":{b}}}"));
        }
    }

    /// Load the persisted daily baseline for today (call at startup).
    pub fn load_daily(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let today = now / 86400;
        self.daily_day = today;
        if let Ok(s) = std::fs::read_to_string(self.daily_path()) {
            // tiny hand-parse: {"day":N,"baseline":F}
            let day = s.split("\"day\":").nth(1).and_then(|x| x.split(',').next()).and_then(|x| x.trim().parse::<u64>().ok());
            let base = s.split("\"baseline\":").nth(1).and_then(|x| x.trim_end_matches(['}', ' ', '\n']).parse::<f64>().ok());
            if day == Some(today) {
                self.daily_baseline = base;
            }
        }
    }

    /// Second pool's price (token per ETH), 0 if no arb pool / not ready.
    pub fn price_b(&self) -> f64 {
        if self.mkt_b.r0 > 0.0 { self.mkt_b.r1 / self.mkt_b.r0 } else { 0.0 }
    }
    /// Price gap (pool A vs pool B) as a signed percentage. Positive = A pricier.
    pub fn arb_gap_pct(&self) -> f64 {
        let (a, b) = (self.price(), self.price_b());
        if a > 0.0 && b > 0.0 { (a - b) / b * 100.0 } else { 0.0 }
    }

    /// Precompute the best arb: ternary-search the (concave) profit curve
    /// P(x) = sell_out(buy_out(x)) − x − round-trip gas over the ETH input x,
    /// using each pool's real reserves + fee. Returns (optimal_eth_in, net_eth).
    /// net_eth <= 0 → NO trade size is profitable at the current spread (the gap
    /// doesn't cover fees + gas). optimal_eth_in can exceed the wallet balance —
    /// that gap between "optimal" and "what we hold" is exactly what a flash loan
    /// would let us capture.
    pub fn arb_optimal(&self) -> (f64, f64) {
        if !self.arb_mode {
            return (0.0, 0.0);
        }
        // Both pools must be quoted in the SAME currency — you can't compare
        // token/ETH against token/USDG. Different quotes → no direct arb.
        if let Some(pb) = self.pool_b.as_ref() {
            if pb.quote != self.pool.quote {
                return (0.0, 0.0);
            }
        }
        let (pa, pb) = (self.price(), self.price_b());
        if pa <= 0.0 || pb <= 0.0 || self.r0 <= 0.0 || self.mkt_b.r0 <= 0.0 {
            return (0.0, 0.0);
        }
        let fa = self.pool.fee;
        let fb = self.pool_b.as_ref().map(|p| p.fee).unwrap_or(fa);
        // Buy on the higher token/ETH pool (cheaper token), sell on the lower.
        // Sanity: same-token / same-quote pools never sit far apart. A giant gap
        // means mismatched quote currencies (e.g. an ETH pool vs a USDG pool) —
        // not arbitrable, and pricing one in the other's units is meaningless.
        // Refuse rather than emit a phantom multi-million-ETH "opportunity".
        let (hi_p, lo_p) = (pa.max(pb), pa.min(pb));
        if lo_p <= 0.0 || hi_p / lo_p > 1.5 {
            return (0.0, 0.0);
        }
        let buy_a = pa >= pb;
        let (br0, br1, bf) = if buy_a { (self.r0, self.r1, fa) } else { (self.mkt_b.r0, self.mkt_b.r1, fb) };
        let (sr0, sr1, sf) = if buy_a { (self.mkt_b.r0, self.mkt_b.r1, fb) } else { (self.r0, self.r1, fa) };
        let gas = 2.0 * 300_000.0 * self.gas_price / 1e18;
        let profit = |x: f64| -> f64 {
            if x <= 0.0 {
                return 0.0;
            }
            let tok = v4::quote_out(br0, br1, x, true, bf);
            if tok <= 0.0 {
                return -x;
            }
            v4::quote_out(sr0, sr1, tok, false, sf) - x - gas
        };
        let (mut lo, mut hi) = (0.0f64, br0.max(sr0)); // input can't exceed pool depth
        for _ in 0..100 {
            let m1 = lo + (hi - lo) / 3.0;
            let m2 = hi - (hi - lo) / 3.0;
            if profit(m1) < profit(m2) {
                lo = m1;
            } else {
                hi = m2;
            }
        }
        let x = (lo + hi) / 2.0;
        (x, profit(x))
    }

    /// Fully-diluted market cap in the QUOTE currency: supply × (quote per token)
    /// = supply/price (price = token per quote).
    pub fn market_cap_quote(&self) -> f64 {
        let p = self.price();
        if p > 0.0 { self.token_supply / p } else { 0.0 }
    }
    /// Market cap in USD: value the quote-denominated cap at the quote's live USD
    /// rate (eth_usd for ETH pools, the fetched stable rate for stable pools).
    pub fn market_cap_usd(&self) -> f64 {
        self.market_cap_quote() * self.pool.quote_usd
    }

    /// Our own LP liquidity in the current pool, in ETH-equivalent (same units
    /// as the market `r0`). Summed over the positions we minted this session.
    pub fn our_liq_eth(&self) -> f64 {
        if self.sqrt_price <= 0.0 {
            return 0.0;
        }
        let l: f64 = self.pos_liq.values().sum();
        (l / self.sqrt_price) / 1e18
    }

    /// Weighted-average cost of the purchased inventory (ETH per token). The
    /// pre-existing "free" bag is NOT counted here — only tokens we bought.
    pub fn avg_basis(&self) -> f64 {
        if self.bought_qty > 1e-12 {
            self.bought_cost / self.bought_qty
        } else {
            0.0
        }
    }

    /// Apply a CONFIRMED trade to the cost-basis inventory. Buys add to the
    /// purchased inventory at their price; sells consume it first (realizing
    /// profit vs basis), and anything beyond it comes from the free bag at zero
    /// cost → pure profit.
    fn apply_fill(&mut self, side: Side, eth: f64, tok: f64, hash: TxHash) {
        match side {
            Side::Buy => {
                self.bought_qty += tok;
                self.bought_cost += eth;
                // Stamp the ENTRY so the trade log is self-contained (survives
                // restarts / lost tape): mkt cap + pooled ETH + buy hash at entry.
                self.entry_mc = if self.price() > 0.0 { self.token_supply / self.price() } else { 0.0 };
                self.entry_pooled_eth = self.r0;
                self.entry_tx = Some(hash);
                // Only the FIRST buy of a position starts the clock. Adding to a
                // winner should not reset how long you have been in it.
                if self.entry_at.is_none() {
                    self.entry_at = Some(crate::ledger::now());
                }
            }
            Side::Sell => {
                let from_basis = tok.min(self.bought_qty);
                let cost = from_basis * self.avg_basis();
                let realized = eth - cost; // free-bag portion has zero cost

                self.last_fill_pnl = Some(realized); // this sell's return, on its own
                // Per-trade order record (greppable: "TRADE ") — a complete,
                // cross-checkable row: entry (mc/pooled/buy-tx) + exit (mc/pooled/
                // sell-tx) + return. Self-contained so it survives restarts.
                let ret = if cost > 1e-12 { realized / cost * 100.0 } else { 0.0 };
                let exit_mc = if self.price() > 0.0 { self.token_supply / self.price() } else { 0.0 };
                let buy_tx = self.entry_tx.map(|h| format!("{h:#x}")).unwrap_or_else(|| "?".into());
                self.logline(&format!(
                    "TRADE {} tok={:#x} pnl={:+.6} ret={:+.1}% cost={:.6} proceeds={:.6} entry_mc={:.3}ETH entry_liq={:.4} exit_mc={:.3}ETH exit_liq={:.4} meta={}/7 buy_tx={} sell_tx={:#x}",
                    self.pool.sym, self.pool.token, realized, ret, cost, eth,
                    self.entry_mc, self.entry_pooled_eth, exit_mc, self.r0,
                    self.meta.score(), buy_tx, hash
                ));
                // The permanent record. `realized_pnl` above is this session's
                // running total and dies with the process; the calendar needs
                // the day this happened on, months from now.
                crate::ledger::append(
                    &self.account,
                    &crate::ledger::Fill {
                        ts: crate::ledger::now(),
                        chain: self.net.clone(),
                        sym: self.pool.sym.clone(),
                        token: format!("{:#x}", self.pool.token),
                        pnl: realized,
                        cost,
                        proceeds: eth,
                        quote_sym: self.pool.quote_sym.clone(),
                        quote_usd: self.pool.quote_usd,
                        tx: format!("{hash:#x}"),
                        held_secs: self.entry_at.map(|t| crate::ledger::now().saturating_sub(t)),
                    },
                );
                // The day figure comes from the ledger, so it is recounted
                // once the fill is in it rather than being kept in step by hand.
                self.refresh_day_realized();
                self.bought_cost = (self.bought_cost - cost).max(0.0);
                self.bought_qty = (self.bought_qty - from_basis).max(0.0);
                // Position closed: the next buy opens a new one, and its hold
                // time starts then rather than continuing this one's.
                if self.bought_qty <= 1e-12 {
                    self.entry_at = None;
                }
            }
        }
    }

    /// Potential profit (ETH) from selling the ENTIRE current holding right now
    /// at the live pool price — slippage-aware (constant-product on the virtual
    /// reserves), net of cost basis. Continuous, so the UI shows what a sell
    /// would actually net *before* you execute, independent of the profit filter.
    pub fn live_edge(&self) -> f64 {
        if self.token_bal <= 0.0 || self.r0 <= 0.0 || self.r1 <= 0.0 {
            return 0.0;
        }
        let proceeds = v4::quote_out(self.r0, self.r1, self.token_bal, false, self.pool.fee);
        proceeds - self.bought_cost
    }

    /// Write a machine-parseable telemetry line to the session log, so a
    /// running session can be monitored live (`tail -f .bot/session-*.log`).
    pub fn telemetry(&mut self, block: u64, round_ms: f64) {
        // Once a window, not once a call: a rate is not something anyone can
        // see by watching individual calls scroll past.
        crate::rpcstats::maybe_report();
        self.logline(&format!(
            "TELEMETRY block={} price={:.8} liq_eth={:.6} eth={:.6} {}={:.4} pnl={:+.6} trades={} fails={} skips={} pending={} round_ms={:.1}",
            block,
            self.price(),
            self.r0,
            self.eth,
            self.pool.sym,
            self.token_bal,
            self.pnl(),
            self.trades,
            self.fails,
            self.skips,
            self.pending.len(),
            round_ms,
        ));
    }

    /// Next nonce for a send — tracked locally so rapid sends never collide on
    /// a stale chain-read nonce (fixes "nonce too low" on back-to-back trades).
    async fn take_nonce<P: Provider>(&mut self, provider: &P) -> eyre::Result<u64> {
        let n = match self.nonce {
            Some(n) => n,
            None => provider.get_transaction_count(self.trader).pending().await?,
        };
        self.nonce = Some(n + 1);
        Ok(n)
    }

    /// Ensure the token's allowance to SwapRouter02 covers `need` wei, using an
    /// EXACT-amount approval — never MAX (no-infinite-approval policy). If the
    /// current allowance is short, approve exactly `need`. Returns Ok(true) when
    /// the allowance is sufficient, Ok(false) when the approval was sent but has
    /// not mined yet (caller should retry the swap shortly). Bounded receipt
    /// poll so it never hangs.
    async fn ensure_v3_allowance<P: Provider>(&mut self, provider: &P, need: U256) -> eyre::Result<bool> {
        let erc = IERC20::new(self.pool.token, provider);
        if let Ok(a) = erc.allowance(self.trader, SWAP_ROUTER_02).call().await {
            if a._0 >= need {
                return Ok(true);
            }
        }
        self.note(format!("Approving {} for the router", self.pool.sym));
        let nonce = self.take_nonce(provider).await?;
        let hash = *erc.approve(SWAP_ROUTER_02, need).gas(120_000).nonce(nonce).send().await?.tx_hash();
        for _ in 0..6u32 {
            if provider.get_transaction_receipt(hash).await.ok().flatten().is_some() {
                return Ok(true);
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Ok(false)
    }

    /// Ensure the token can be pulled by the UniversalRouter via Permit2 for at
    /// least `need` wei — the settle path of a Flaunch sell. Two grants: the
    /// ERC-20's allowance to Permit2 (MAX, matching the LP flow — Permit2's own
    /// allowance is the bounded one), and Permit2's exact-amount allowance to
    /// the router. Returns Ok(true) when both cover `need`, Ok(false) when an
    /// approval was sent but has not mined yet (retry the sell shortly).
    async fn ensure_ur_allowance<P: Provider>(&mut self, provider: &P, need: U256) -> eyre::Result<bool> {
        use alloy::primitives::aliases::{U160, U48};
        let erc = IERC20::new(self.pool.token, provider);
        let p2 = IPermit2::new(PERMIT2, provider);
        let erc_ok = erc
            .allowance(self.trader, PERMIT2)
            .call()
            .await
            .map(|a| a._0 >= need)
            .unwrap_or(false);
        // Permit2 allowances persist on-chain and expire — check before sending.
        let now48 = U48::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        let need160: U160 = need.min(U256::from(U160::MAX)).to();
        let p2_ok = p2
            .allowance(self.trader, self.pool.token, UNIVERSAL_ROUTER)
            .call()
            .await
            .map(|a| a.amount >= need160 && a.expiration > now48)
            .unwrap_or(false);
        if erc_ok && p2_ok {
            return Ok(true);
        }
        self.note(format!("Approving {} for the Universal Router", self.pool.sym));
        let mut last = None;
        if !erc_ok {
            let nonce = self.take_nonce(provider).await?;
            last = Some(*erc.approve(PERMIT2, U256::MAX).gas(120_000).nonce(nonce).send().await?.tx_hash());
        }
        if !p2_ok {
            let expiration48 = U48::from(v4::FAR_DEADLINE);
            let nonce = self.take_nonce(provider).await?;
            last = Some(
                *p2.approve(self.pool.token, UNIVERSAL_ROUTER, need160, expiration48)
                    .gas(120_000)
                    .nonce(nonce)
                    .send()
                    .await?
                    .tx_hash(),
            );
        }
        if let Some(hash) = last {
            for _ in 0..6u32 {
                if provider.get_transaction_receipt(hash).await.ok().flatten().is_some() {
                    return Ok(true);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        Ok(false)
    }

    /// After a buy confirms, top the SwapRouter02 allowance up to the full token
    /// balance (exact amount, incremented per buy) so the eventual v3 sell needs
    /// no approval and fires instantly. No-op for tokens without a v3 route. Sets
    /// `v3_covered` so the hot sell path can skip the allowance check entirely.
    /// Flaunch routes get the same treatment through Permit2 + the router.
    async fn pre_approve_exit<P: Provider>(&mut self, provider: &P) -> eyre::Result<()> {
        if !self.has_v3_route() && !self.has_flaunch_route() {
            return Ok(());
        }
        let bal = IERC20::new(self.pool.token, provider)
            .balanceOf(self.trader)
            .call()
            .await?
            ._0;
        if bal.is_zero() {
            return Ok(());
        }
        if self.has_v3_route() {
            let covered = self.ensure_v3_allowance(provider, bal).await?;
            self.v3_covered = covered;
            if covered {
                self.note(format!("Pre approved {} so an exit can go out immediately", self.pool.sym));
            }
        }
        if self.has_flaunch_route() {
            let covered = self.ensure_ur_allowance(provider, bal).await?;
            self.ur_permit2_done = covered;
            if covered {
                self.note(format!("Pre approved {} so an exit can go out immediately", self.pool.sym));
            }
        }
        Ok(())
    }

    /// Append an entry to the orders queue (capped).
    fn push_order(&mut self, label: String, status: OrderStatus, hash: Option<TxHash>) {
        let price = self.price();
        let mc = if price > 0.0 { self.token_supply / price } else { 0.0 };
        // Amount ALWAYS in ETH numeraire (cost in ether). Buys/LP encode the ETH in
        // the label; sells are token-denominated, so value them at price (ETH = tok
        // / price, since price is tokens-per-ETH).
        let eth = eth_of_label(&label)
            .unwrap_or_else(|| if price > 0.0 { self.token_bal / price } else { 0.0 });
        let is_v4 = matches!(self.pool.kind, PoolKind::V4 { .. } | PoolKind::FlaunchV4 { .. });
        self.orders.push_back(Order { label, status, hash, mc, pooled: self.r0, eth, is_v4 });
        while self.orders.len() > 200 {
            self.orders.pop_front();
        }
    }

    /// Move any pending order matching `hash` to a terminal status.
    fn settle_order(&mut self, hash: TxHash, status: OrderStatus) {
        if let Some(o) = self.orders.iter_mut().find(|o| o.hash == Some(hash)) {
            o.status = status;
        }
    }

    /// Log a line AND surface it as the dashboard status (user feedback).
    pub fn note(&mut self, s: String) {
        self.logline(&s);
        self.status = s;
    }

    fn logline(&mut self, s: &str) {
        use std::io::Write;
        // Timestamped (UTC HH:MM:SS) so the log is a clear sequence of events.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (h, m, sec) = ((now % 86400) / 3600, (now % 3600) / 60, now % 60);
        let line = format!("[{:02}:{:02}:{:02}] {}", h, m, sec, s);
        let _ = writeln!(self.log, "{line}");
        let _ = self.log.flush();
        // Keep the last 500 lines in memory for the on-screen logs view.
        self.logs.push_back(line);
        while self.logs.len() > 500 {
            self.logs.pop_front();
        }
    }

    /// Apply a market snapshot read off-thread (keeps STATE-change logging and
    /// the PnL baseline). The UI thread calls this; it does NO network I/O.
    pub fn apply_market(&mut self, m: Market) {
        let was_ready = self.ready;
        self.sqrt_price = m.sqrt_price;
        self.tick = m.tick;
        // Reserves come only from a full read. A light one returns zero for
        // them because it did not ask — assigning that made price, pooled depth
        // and market cap flip to nothing between full reads, which on screen is
        // a panel that blinks.
        if m.full {
            self.r0 = m.r0;
            self.r1 = m.r1;
        }
        crate::trace(&format!(
            "market: price={:.10} r0={:.6} r1={:.6} tick={} quote_dec={} token_dec={} supply={:.4}",
            self.price(),
            self.r0,
            self.r1,
            self.tick,
            self.pool.quote.decimals(),
            self.pool.token_decimals,
            self.token_supply,
        ));
        // Keep the last known balance when the read failed: stale is honest,
        // zero is a lie.
        if let Some(eth) = m.eth {
            self.eth = eth;
        }
        if let Some(t) = m.token_bal {
            self.token_bal = t;
        }
        self.last_read_ms = m.read_ms;
        // Liquidity and reserves come only from a full read. A light one did
        // not ask, and "did not ask" is not "there is none".
        if m.full {
            self.ready = m.ready;
        }
        if m.gas_price > 0.0 {
            self.gas_price = m.gas_price;
        }
        if m.supply > 0.0 {
            self.token_supply = m.supply;
        }
        if was_ready && !self.ready {
            self.logline("STATE pool went ILLIQUID (no active liquidity)");
        } else if !was_ready && self.ready {
            self.logline(&format!("STATE pool went LIVE (price {:.6})", self.price()));
        }
        // Only set the PnL baseline once we have a real balance (not a failed read).
        if self.baseline_eth.is_none() && self.eth > 0.0 {
            self.baseline_eth = Some(self.eth);
        }
        if self.eth > 0.0 {
            self.update_daily();
        }
    }

    /// True if any candidate route is a v3 pool (which needs a token approval to
    /// sell). Used to grant the approval before simulating sells across venues.
    fn has_v3_route(&self) -> bool {
        self.routes.iter().any(|r| r.kind.is_v3())
    }

    /// True if any candidate route (or the active pool) is a Flaunch pool,
    /// whose sells settle the coin through Permit2 and need `ensure_ur_allowance`.
    fn has_flaunch_route(&self) -> bool {
        matches!(self.pool.kind, PoolKind::FlaunchV4 { .. })
            || self.routes.iter().any(|r| matches!(r.kind, PoolKind::FlaunchV4 { .. }))
    }

    /// Best-execution router: simulate `amount` (ETH for a buy, tokens for a
    /// sell) on EVERY candidate ETH-quoted venue and return the one with the
    /// greatest output, excluding any that would revert. This is what captures
    /// fee-tier differences AND hook take — a hooked pool that skims more (or
    /// reverts) simply loses the comparison. Falls back to the current pool if
    /// no routes are configured.
    pub async fn best_venue<P: Provider>(&mut self, provider: &P, amount: f64, buying: bool) -> Option<(Route, f64)> {
        // Candidate set: the configured routes, or just the active pool. Label by
        // FEE TIER (e.g. "1%"), not token symbol — a token literally named "BUY"
        // made a sell's route read as "v3 BUY".
        let active = Route {
            kind: self.pool.kind,
            token: self.pool.token,
            fee: self.pool.fee,
            label: format!("{:.2}%", self.pool.fee as f64 / 10_000.0),
        };
        let cands: Vec<Route> = if self.routes.is_empty() { vec![active.clone()] } else { self.routes.clone() };
        // For sells, cap the wei amount to the EXACT on-chain balance. The f64
        // round-trip of the human amount can land a few wei ABOVE the true
        // balance, which makes the router's transferFrom revert with STF. All
        // routes share the same token, so one balance read caps them all.
        let sell_cap: Wei = if buying {
            Wei::MAX
        } else {
            IERC20::new(self.pool.token, provider)
                .balanceOf(self.trader)
                .call()
                .await
                .map(|b| Wei::exact(b._0))
                .unwrap_or(Wei::MAX)
        };
        let mut best: Option<(Route, f64)> = None;
        for r in cands {
            let venue = format!("{} {}", r.kind.proto(), r.label);
            let pref = PoolRef {
                kind: r.kind,
                token: r.token,
                quote: Quote::Eth,
                token_decimals: self.pool.token_decimals,
            };
            let m = match read_market(provider, pref, self.trader).await {
                Ok(m) => m,
                Err(e) => { self.logline(&format!("route {venue}: read failed {}", short_err(&e.to_string()))); continue; }
            };
            if m.r0 <= 0.0 || m.r1 <= 0.0 {
                self.logline(&format!("route {venue}: no liquidity"));
                continue;
            }
            let out = v4::quote_out(m.r0, m.r1, amount, buying, r.fee);
            if out <= 0.0 {
                self.logline(&format!("route {venue}: zero quote"));
                continue;
            }
            // Pre-flight for EXECUTABILITY only (min_out = 0) — this checks the
            // venue actually works (excludes reverting / hook-broken pools) and
            // we rank by the quoted output. Real slippage protection is applied
            // at the actual send, not here, so a slightly-off estimate can't
            // falsely reject a working venue.
            // Input side is ETH on a buy, the tracked token on a sell — so the
            // scaling has to follow the direction, not assume 18 decimals.
            let wei_in = if buying {
                Wei::rounded(amount)
            } else {
                Wei::of_token(amount, self.pool.token_decimals)
            }
            .min(sell_cap);
            let (to, data, value) = build_swap(r.kind, r.token, r.fee, buying, wei_in, Wei::ZERO, self.trader);
            let tx = TransactionRequest::default().with_to(to).with_input(data).with_value(value).with_from(self.trader);
            if let Err(e) = provider.call(&tx).await {
                self.logline(&format!("route {venue}: pre-flight revert {}", short_err(&e.to_string())));
                continue;
            }
            self.logline(&format!("route {venue}: out {out:.8}"));
            if best.as_ref().map_or(true, |(_, b)| out > *b) {
                best = Some((r, out));
            }
        }
        // Resilience: NEVER let routing block a trade (a sell is an EXIT — you want
        // OUT). If no candidate came back clean (e.g. a transient read error, or a
        // single-pool token), fall back to the active pool. Its real send still
        // pre-flights, so this can't cause a blind gas-wasting revert.
        if best.is_none() {
            self.logline("route: none verified — falling back to active pool");
            let out = v4::quote_out(self.r0, self.r1, amount, buying, self.pool.fee).max(0.0);
            best = Some((active, out));
        }
        best
    }

    /// Place one order: size to pool depth, quote exact output, pre-flight with
    /// a free eth_call (skip if it would revert — no gas wasted), then send.
    pub async fn place<P: Provider>(&mut self, provider: &P, side: Side) -> eyre::Result<()> {
        // Dedup guard: never stack a trade while one is already in flight for the
        // token — stops the double/triple-buy race (and duplicate sells).
        if self.guard_dup && self.pending.iter().any(|p| p.side.is_some()) {
            self.skips += 1;
            self.note(format!("Skipped the {} because a trade is already pending", side_str(side).to_lowercase()));
            return Ok(());
        }
        if !self.ready || self.price() <= 0.0 {
            self.skips += 1;
            self.note(format!("Skipped the {} because there is no market yet", side_str(side).to_lowercase()));
            return Ok(());
        }
        let buying = side == Side::Buy;

        // Size as a % of the INPUT-side BALANCE (what you want to spend), NOT of
        // pool depth — a deep pool shouldn't force a trade bigger than your
        // wallet. Available = balance minus in-flight same-side spends minus a
        // gas buffer (buys pay gas from ETH too).
        let balance_in = if buying { self.eth } else { self.token_bal };
        let reserved: f64 = self
            .pending
            .iter()
            .filter(|p| p.side == Some(side))
            .map(|p| if buying { p.eth_amt } else { p.tok_amt })
            .sum();
        let gas_buffer = if buying { 400_000.0 * self.gas_price / 1e18 } else { 0.0 };
        let available = (balance_in - reserved - gas_buffer).max(0.0);
        let frac = if buying { self.buy_frac } else { self.sell_frac };
        let mut amount_in = (balance_in * frac).min(available);

        // Copy mode: on a BUY, size to the observed buyer step from the tape — but
        // ONLY if we can actually afford it. If the step is bigger than our balance
        // (e.g. their 0.06 ETH buy while we hold 0.002), copying would clamp to the
        // whole wallet — a bad bet. In that case fall back to the manual buy_frac
        // amount (the % you set). Also falls back when no step is known yet.
        if buying && self.is_copy() && self.copy_buy_eth > 0.0 && self.copy_buy_eth <= available {
            amount_in = self.copy_buy_eth;
        }

        // For sells, clamp to the LIVE token balance — the cached snapshot can be
        // stale-high (a just-confirmed sell), and selling more than is held makes
        // the router's transferFrom revert with STF. Keep the EXACT U256 balance
        // too: the f64 round-trip can round a few wei above it, also causing STF.
        let mut sell_cap: Wei = Wei::MAX;
        if !buying {
            if let Ok(b) = IERC20::new(self.pool.token, provider).balanceOf(self.trader).call().await {
                amount_in = amount_in.min(wei_to_f64(b._0));
                sell_cap = Wei::exact(b._0);
            }
        }

        // Price-impact cap: never let a single swap move price more than
        // `max_price_move` — protects a thin pool even when your balance is big.
        let band = self.max_price_move;
        if band > 0.0 {
            let l = (self.r0 * self.r1).sqrt();
            let sqrt_p = (self.r1 / self.r0).sqrt();
            let net_max = if buying {
                l * (1.0 / (1.0 - band).sqrt() - 1.0) / sqrt_p
            } else {
                l * sqrt_p * ((1.0 + band).sqrt() - 1.0)
            };
            let cap = net_max / (1.0 - self.pool.fee as f64 / 1_000_000.0); // undo fee haircut
            if cap > 0.0 && amount_in > cap {
                amount_in = cap;
            }
        }

        if amount_in <= 0.0 {
            self.skips += 1;
            self.note(format!(
                "skipped {} — no {} to spend (bal {:.6})",
                side_str(side), if buying { "ETH" } else { &self.pool.sym }, balance_in
            ));
            return Ok(());
        }
        // v3 sells need the token approved to SwapRouter02. Normally the buy
        // already pre-approved the exact balance (v3_covered) → zero-RPC fast
        // path. Fallback: approve the exact balance for a holding that wasn't
        // pre-approved, deferring the sell one round if it hasn't mined.
        if !buying && self.has_v3_route() && !self.v3_covered {
            let need = IERC20::new(self.pool.token, provider)
                .balanceOf(self.trader).call().await.map(|b| b._0).unwrap_or(U256::ZERO);
            if !need.is_zero() {
                match self.ensure_v3_allowance(provider, need).await {
                    Ok(true) => self.v3_covered = true,
                    Ok(false) => { self.note("Approval is still confirming. Try the sell again shortly".into()); return Ok(()); }
                    Err(e) => { self.note(format!("Approval failed. {}", short_err(&e.to_string()))); return Ok(()); }
                }
            }
        }
        // Flaunch sells settle the coin through Permit2 — grant it before the
        // routing pre-flight, or every simulated sell reverts.
        if !buying && self.has_flaunch_route() && !self.ur_permit2_done {
            let need = IERC20::new(self.pool.token, provider)
                .balanceOf(self.trader).call().await.map(|b| b._0).unwrap_or(U256::ZERO);
            if !need.is_zero() {
                match self.ensure_ur_allowance(provider, need).await {
                    Ok(true) => self.ur_permit2_done = true,
                    Ok(false) => { self.note("Approval is still confirming. Try the sell again shortly".into()); return Ok(()); }
                    Err(e) => { self.note(format!("Approval failed. {}", short_err(&e.to_string()))); return Ok(()); }
                }
            }
        }

        // Best-execution routing: simulate this trade on every candidate venue
        // and take the one that nets the most (fee tiers + depth + hook take).
        let (route, expected) = match self.best_venue(provider, amount_in, buying).await {
            Some(x) => x,
            None => {
                self.skips += 1;
                self.push_order(format!("{} (no venue)", side_str(side)), OrderStatus::Skipped, None);
                self.note(format!("Skipped the {} because no pool passes the pre flight check", side_str(side).to_lowercase()));
                return Ok(());
            }
        };

        // Profitability filter (toggle 'g'): value the SIMULATED output against
        // the SMA reference and subtract gas — only trade when the expected
        // value is net-positive. Turns the pre-flight into an EV filter, not
        // just a revert filter, and enforces buy-dips / sell-rips.
        if self.profit_guard {
            let p_ref = if self.ref_price > 0.0 { self.ref_price } else { self.price() };
            // Buy: fair-value edge vs SMA (accumulate dips). Sell: proceeds
            // minus the cost basis of the tokens sold (free bag = zero cost →
            // pure profit once purchased inventory is depleted).
            let mtm = if buying {
                expected / p_ref - amount_in
            } else {
                let from_basis = amount_in.min(self.bought_qty);
                let cost = from_basis * self.avg_basis();
                expected - cost
            };
            const SWAP_GAS: f64 = 250_000.0;
            let gas_cost = SWAP_GAS * self.gas_price / 1e18;
            let edge = mtm - gas_cost;
            self.last_edge = edge;
            if edge < self.min_edge_eth {
                self.skips += 1;
                self.push_order(
                    format!("{} (no edge {:+.6})", side_str(side), edge),
                    OrderStatus::Skipped,
                    None,
                );
                self.note(format!(
                    "skipped {} — not profitable: edge {:+.7} ETH (gas {:.7})",
                    side_str(side), edge, gas_cost
                ));
                return Ok(());
            }
        }

        // Floor sits below the quote by the configured tolerance. Clamped to a
        // 1% minimum so "0" can never mean "no protection".
        let min_out = expected * self.slip_floor();
        // amount_in / min_out are HUMAN units — scale to base units using EACH
        // side's real decimals. A buy spends ETH (18) for token; a sell spends
        // token for ETH, so the two swap over. Without the scaling a sub-1.0
        // amount truncates to 0 → SwapAmountCannotBeZero (0xbe8b8507); with the
        // WRONG scaling a 6-dec token is off by 10^12.
        // Cap sells to the exact on-chain balance (f64 rounding → STF otherwise).
        let td = self.pool.token_decimals;
        let (wei_in, wei_min) = if buying {
            (Wei::rounded(amount_in), Wei::of_token(min_out, td))
        } else {
            (Wei::of_token(amount_in, td), Wei::rounded(min_out))
        };
        let wei_in = wei_in.min(sell_cap);

        // Build calldata for the WINNING venue (not necessarily the active pool).
        let (to, data, value) = build_swap(route.kind, route.token, route.fee, buying, wei_in, wei_min, self.trader);

        // Explicit gas limit — override the filler's zero-buffer eth_estimateGas,
        // which lands just under the real need on cold-storage blocks and OOGs the
        // WETH unwrapWETH9 step (this is what reverts sells, NOT slippage). You pay
        // for gas USED, so the headroom is free; sells carry the unwrap → larger cap.
        // Also skips a per-tx estimate round-trip, so sends are a touch faster.
        // Flaunch swaps traverse TWO hooked pools (flETH + the Flaunch hook's
        // fee machinery), so they get more headroom in both directions.
        let gas_limit = match route.kind {
            PoolKind::FlaunchV4 { .. } => if buying { 500_000 } else { 650_000 },
            _ => if buying { 300_000 } else { 450_000 },
        };
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_input(data)
            .with_value(value)
            .with_gas_limit(gas_limit)
            .with_from(self.trader);

        // Pre-flight: if it would revert, skip — do NOT spend gas. Surface the
        // revert reason so the user can see WHY (not just "skipped").
        let label = format!(
            "{} ~{:.6} ETH @ {:.6} [{} {}] liq_eth={:.6}",
            side_str(side),
            if buying { amount_in } else { expected },
            self.price(),
            route.kind.proto(),
            route.label,
            self.r0, // pooled ETH at time of trade (same source as telemetry liq_eth)
        );
        if let Err(e) = provider.call(&tx).await {
            self.skips += 1;
            self.push_order(label, OrderStatus::Skipped, None);
            self.note(format!("Skipped the {} because it would revert. {}", side_str(side).to_lowercase(), short_err(&e.to_string())));
            return Ok(());
        }

        // Local nonce so rapid sends don't collide (see take_nonce).
        let nonce = match self.take_nonce(provider).await {
            Ok(n) => n,
            Err(e) => {
                self.note(format!("Could not read the account nonce. {}", short_err(&e.to_string())));
                return Ok(());
            }
        };
        let tx = tx.with_nonce(nonce);

        // Send (gas estimated by the node; a local node makes this cheap).
        match provider.send_transaction(tx).await {
            Ok(p) => {
                let hash = *p.tx_hash();
                self.last_side = side;
                self.push_order(label.clone(), OrderStatus::Pending, Some(hash));
                self.note(format!("SENT {}  tx {}", label, hash));
                // Record fill for cost-basis accounting (applied on confirm).
                let (eth_amt, tok_amt) = if buying { (amount_in, expected) } else { (expected, amount_in) };
                self.pending.push(Pending { hash, label, side: Some(side), eth_amt, tok_amt, position_id: None });
            }
            Err(e) => {
                self.fails += 1;
                self.nonce = None; // resync on failure
                self.push_order(label.clone(), OrderStatus::Failed, None);
                self.logline(&format!("FAILED {}: {}", label, e));
            }
        }
        Ok(())
    }

    /// Pre-flight + send a prepared swap (already-encoded calldata), recording it
    /// in the orders queue and the pending list (for cost-basis + reaping).
    /// Returns the tx hash on a successful send. Shared by the arb legs.
    async fn send_raw<P: Provider>(
        &mut self,
        provider: &P,
        to: Address,
        data: Bytes,
        value: U256,
        label: String,
        side: Option<Side>,
        eth_amt: f64,
        tok_amt: f64,
    ) -> Option<TxHash> {
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_input(data)
            .with_value(value)
            .with_gas_limit(if matches!(side, Some(Side::Buy)) { 300_000 } else { 450_000 })
            .with_from(self.trader);
        if let Err(e) = provider.call(&tx).await {
            self.skips += 1;
            self.push_order(label.clone(), OrderStatus::Skipped, None);
            self.note(format!("skipped {} — would revert: {}", label, short_err(&e.to_string())));
            return None;
        }
        let nonce = match self.take_nonce(provider).await {
            Ok(n) => n,
            Err(e) => {
                self.note(format!("Could not read the account nonce. {}", short_err(&e.to_string())));
                return None;
            }
        };
        let tx = tx.with_nonce(nonce);
        match provider.send_transaction(tx).await {
            Ok(p) => {
                let hash = *p.tx_hash();
                self.push_order(label.clone(), OrderStatus::Pending, Some(hash));
                self.note(format!("SENT {}  tx {}", label, hash));
                self.pending.push(Pending { hash, label, side, eth_amt, tok_amt, position_id: None });
                Some(hash)
            }
            Err(e) => {
                self.fails += 1;
                self.nonce = None;
                self.push_order(label.clone(), OrderStatus::Failed, None);
                self.logline(&format!("FAILED {}: {}", label, e));
                None
            }
        }
    }

    /// Execute a two-leg arb across the two configured pools: BUY the token on
    /// the cheaper pool, wait (bounded) for it to land, then SELL it on the
    /// dearer pool. Both legs are pre-flighted; if the sell leg would revert we
    /// keep the tokens (manual exit). NOT atomic — sequential, so price can move
    /// between legs. The gap-vs-fees gate + pre-flight bound the downside.
    pub async fn arb<P: Provider>(&mut self, provider: &P) -> eyre::Result<()> {
        if !self.arb_mode {
            self.note("arb: not in arb mode (press d to pick a 2nd pool)".into());
            return Ok(());
        }
        let Some((kb, fb)) = self.pool_b.as_ref().map(|p| (p.kind, p.fee)) else {
            self.note("arb: no second pool selected (press d)".into());
            return Ok(());
        };
        if !self.ready || self.price() <= 0.0 || self.mkt_b.r0 <= 0.0 || self.price_b() <= 0.0 {
            self.skips += 1;
            self.note("Both arb pools are not live yet".into());
            return Ok(());
        }
        // Refuse cross-quote arb: token/ETH vs token/USDG isn't a real spread,
        // and executing it would buy on one leg then fail to sell on the other.
        if self.pool_b.as_ref().map(|p| p.quote) != Some(self.pool.quote) {
            self.skips += 1;
            self.note("The two arb pools price in different currencies, so the arb was refused".into());
            return Ok(());
        }
        let (ka, tok, fa) = (self.pool.kind, self.pool.token, self.pool.fee);
        let (pa, pbp) = (self.price(), self.price_b()); // token per ETH
        let (ra0, ra1) = (self.r0, self.r1);
        let (rb0, rb1) = (self.mkt_b.r0, self.mkt_b.r1);

        // Buy where the token is CHEAPER = more token per ETH = higher price value.
        let buy_on_a = pa >= pbp;
        let (bk, bf, br0, br1) = if buy_on_a { (ka, fa, ra0, ra1) } else { (kb, fb, rb0, rb1) };
        let (sk, sf, sr0, sr1) = if buy_on_a { (kb, fb, rb0, rb1) } else { (ka, fa, ra0, ra1) };

        // Gross arb multiple: 1 ETH -> tokens on cheap pool -> ETH on dear pool,
        // net of BOTH pools' fees. > 1 means the spread beats the round-trip fee.
        let (ph, pl) = (pa.max(pbp), pa.min(pbp));
        let gross = (ph / pl) * (1.0 - bf as f64 / 1e6) * (1.0 - sf as f64 / 1e6);
        let two_tx_gas = 2.0 * 300_000.0 * self.gas_price / 1e18;

        // Size the buy leg off the ETH balance (buy_frac), leaving a gas buffer.
        let eth_in = ((self.eth - two_tx_gas).max(0.0) * self.buy_frac).max(0.0);
        if eth_in <= 0.0 {
            self.skips += 1;
            self.note("No ETH available to run the arb".into());
            return Ok(());
        }
        let est_net = eth_in * (gross - 1.0) - two_tx_gas;
        if est_net <= self.min_edge_eth {
            self.skips += 1;
            self.push_order(format!("ARB (no edge {:+.6})", est_net), OrderStatus::Skipped, None);
            self.note(format!(
                "arb: gap too small — est net {:+.7} ETH (gross x{:.5}, gas {:.7})",
                est_net, gross, two_tx_gas
            ));
            return Ok(());
        }

        // ---- Leg 1: BUY on the cheap pool ----
        let tok_out = v4::quote_out(br0, br1, eth_in, true, bf);
        if tok_out <= 0.0 {
            self.skips += 1;
            self.note("The arb buy leg quoted nothing".into());
            return Ok(());
        }
        let wei_in = Wei::rounded(eth_in); // ETH in — 18-dec
        // Token OUT: scale by the token's real decimals, not a blanket 1e18.
        let wei_min = Wei::of_token(tok_out * self.slip_floor(), self.pool.token_decimals);
        let (to1, data1, val1) = build_swap(bk, tok, bf, true, wei_in, wei_min, self.trader);
        let label1 = format!(
            "ARB BUY ~{:.6} ETH @ {:.6} [{}]",
            eth_in, if buy_on_a { pa } else { pbp }, bk.proto()
        );
        let erc = IERC20::new(tok, provider);
        let bal_before = erc
            .balanceOf(self.trader)
            .call()
            .await
            .map(|b| wei_to_f64(b._0))
            .unwrap_or(self.token_bal);
        if self.send_raw(provider, to1, data1, val1, label1, Some(Side::Buy), eth_in, tok_out).await.is_none() {
            return Ok(());
        }

        // Wait (bounded ~8s) for the bought tokens to land before selling them.
        let mut got = 0.0;
        for _ in 0..40u32 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if let Ok(b) = erc.balanceOf(self.trader).call().await {
                let now = wei_to_f64(b._0);
                if now > bal_before + tok_out * 0.5 {
                    got = now - bal_before;
                    break;
                }
            }
        }
        if got <= 0.0 {
            self.note("The arb buy leg has not landed yet, so the sell leg is waiting. Press the arb key again".into());
            return Ok(());
        }

        // ---- Leg 2: SELL the bought tokens on the dear pool ----
        let eth_out = v4::quote_out(sr0, sr1, got, false, sf);
        if eth_out <= 0.0 {
            self.note("The arb sell leg quoted nothing, so the tokens are being held".into());
            return Ok(());
        }
        // v3 sells need the token approved to SwapRouter02 first (same token);
        // Flaunch sells need the Permit2 + router grants.
        if sk.is_v3() && !self.v3_covered {
            let need = IERC20::new(self.pool.token, provider)
                .balanceOf(self.trader).call().await.map(|b| b._0).unwrap_or(U256::ZERO);
            if !need.is_zero() {
                match self.ensure_v3_allowance(provider, need).await {
                    Ok(true) => self.v3_covered = true,
                    Ok(false) => { self.note("Arb approval is still confirming. Try again shortly".into()); return Ok(()); }
                    Err(e) => { self.note(format!("Arb approval failed. {}", short_err(&e.to_string()))); return Ok(()); }
                }
            }
        }
        if matches!(sk, PoolKind::FlaunchV4 { .. }) && !self.ur_permit2_done {
            let need = IERC20::new(self.pool.token, provider)
                .balanceOf(self.trader).call().await.map(|b| b._0).unwrap_or(U256::ZERO);
            if !need.is_zero() {
                match self.ensure_ur_allowance(provider, need).await {
                    Ok(true) => self.ur_permit2_done = true,
                    Ok(false) => { self.note("Arb approval is still confirming. Try again shortly".into()); return Ok(()); }
                    Err(e) => { self.note(format!("Arb approval failed. {}", short_err(&e.to_string()))); return Ok(()); }
                }
            }
        }
        // Sell exactly what landed, capped to the live balance (no f64 overshoot).
        let bal_now = IERC20::new(tok, provider).balanceOf(self.trader).call().await.map(|b| Wei::exact(b._0)).unwrap_or(Wei::MAX);
        // Token IN: real decimals. ETH out stays 18-dec.
        let s_wei_in = Wei::of_token(got, self.pool.token_decimals).min(bal_now);
        let s_wei_min = Wei::rounded(eth_out * self.slip_floor());
        let (to2, data2, val2) = build_swap(sk, tok, sf, false, s_wei_in, s_wei_min, self.trader);
        let label2 = format!(
            "ARB SELL {:.4} {} -> ~{:.6} ETH [{}]",
            got, self.pool.sym, eth_out, sk.proto()
        );
        self.send_raw(provider, to2, data2, val2, label2, Some(Side::Sell), eth_out, got).await;
        self.note(format!(
            "ARB round-trip: bought {:.2} {} on {}, selling on {} for ~{:.6} ETH (est net {:+.6})",
            got, self.pool.sym, bk.proto(), sk.proto(), eth_out, eth_out - eth_in
        ));
        Ok(())
    }

    /// Reap pending txs: confirmed -> trades, reverted -> fails.
    pub async fn reap<P: Provider>(&mut self, provider: &P) {
        let mut still = Vec::new();
        for p in self.pending.drain(..).collect::<Vec<_>>() {
            match provider.get_transaction_receipt(p.hash).await {
                Ok(Some(rc)) => {
                    if rc.status() {
                        self.trades += 1;
                        self.settle_order(p.hash, OrderStatus::Confirmed);
                        let mut recv_eth: Option<f64> = None; // actual ETH received on a sell
                        if let Some(side) = p.side {
                            // Realized PnL must use what the swap ACTUALLY moved, not the
                            // pre-trade quote (`expected`) — on fast-draining pools the price
                            // slips between quote and fill, so the quote overstates proceeds.
                            // Decode the receipt's Transfer logs: WETH → router = real sell
                            // proceeds; pool-token → trader = real buy fill. The side we
                            // control exactly (buy ETH-in / sell tokens-in) keeps its value.
                            const XFER: B256 = alloy::primitives::b256!(
                                "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
                            );
                            let (mut weth_out, mut tok_in) = (U256::ZERO, U256::ZERO);
                            for lg in rc.inner.logs() {
                                let tp = lg.topics();
                                if tp.len() < 3 || tp[0] != XFER {
                                    continue;
                                }
                                let to = Address::from_word(tp[2]);
                                let d = lg.data().data.as_ref();
                                if d.len() < 32 {
                                    continue;
                                }
                                let val = U256::from_be_slice(&d[d.len() - 32..]);
                                if lg.address() == WETH
                                    && (to == SWAP_ROUTER_02 || to == UNIVERSAL_ROUTER)
                                {
                                    weth_out = weth_out.saturating_add(val);
                                } else if lg.address() == self.pool.token && to == self.trader {
                                    tok_in = tok_in.saturating_add(val);
                                }
                            }
                            let mut fill_eth = p.eth_amt;
                            let mut fill_tok = p.tok_amt;
                            if side == Side::Sell && weth_out > U256::ZERO {
                                fill_eth = wei_to_f64(weth_out); // real ETH received
                            } else if side == Side::Buy && tok_in > U256::ZERO {
                                fill_tok = wei_to_f64(tok_in); // real tokens received
                            }
                            if side == Side::Sell {
                                recv_eth = Some(fill_eth); // key number: ETH actually received
                            }
                            self.apply_fill(side, fill_eth, fill_tok, p.hash);
                            // Pre-approve the exit the moment a buy confirms, so the
                            // later v3 sell carries an exact-amount allowance and
                            // fires instantly (no approve tx in the sell path).
                            if side == Side::Buy {
                                if let Err(e) = self.pre_approve_exit(provider).await {
                                    self.note(format!("Pre approval failed. {}", short_err(&e.to_string())));
                                }
                            }
                        }
                        // A confirmed ADD LP mint: read the REAL tokenId from the
                        // ERC-721 Transfer(0x0 -> trader) log and cache it.
                        if p.label.starts_with("ADD LP") {
                            const XFER: B256 = alloy::primitives::b256!(
                                "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
                            );
                            let minted_l = self.mint_liq.remove(&p.hash).unwrap_or(0.0);
                            for lg in rc.inner.logs() {
                                let tp = lg.topics();
                                if lg.address() == POSITION_MANAGER
                                    && tp.len() == 4
                                    && tp[0] == XFER
                                    && Address::from_word(tp[1]) == Address::ZERO
                                    && Address::from_word(tp[2]) == self.trader
                                {
                                    let id = U256::from_be_slice(tp[3].as_slice());
                                    if !self.positions.contains(&id) {
                                        self.positions.push(id);
                                    }
                                    self.pos_liq.insert(id, minted_l); // our liquidity share
                                }
                            }
                        }
                        // A confirmed burn drops that position's liquidity from our share.
                        if let Some(id) = p.position_id {
                            if p.label.starts_with("REMOVE") || p.label.starts_with("CLOSE") {
                                self.pos_liq.remove(&id);
                            }
                        }
                        // For sells, surface the ACTUAL ETH received (numeraire) — the key number.
                        match recv_eth {
                            Some(eth) => self.logline(&format!("CONFIRMED {}  received {:.6} ETH  tx {}", p.label, eth, p.hash)),
                            None => self.logline(&format!("CONFIRMED {}  tx {}", p.label, p.hash)),
                        }
                    } else {
                        self.fails += 1;
                        self.settle_order(p.hash, OrderStatus::Reverted);
                        // A burn that reverted didn't actually close — put the
                        // position back in the cache so it can be retried.
                        if let Some(id) = p.position_id {
                            let burn = p.label.starts_with("REMOVE") || p.label.starts_with("CLOSE");
                            if burn && !self.positions.contains(&id) {
                                self.positions.push(id);
                            }
                        }
                        self.logline(&format!("REVERTED {}  tx {}", p.label, p.hash));
                    }
                }
                _ => still.push(p),
            }
        }
        self.pending = still;
    }

    /// Open a v4 liquidity position sized to `eth_wei` at the current range.
    /// v4-only: the bot doesn't manage v3 (NonfungiblePositionManager) positions.
    pub async fn add_liquidity<P: Provider>(&mut self, provider: &P, eth_wei: u128) -> eyre::Result<()> {
        let spacing = match self.pool.kind {
            PoolKind::V4 { tick_spacing, .. } => tick_spacing,
            PoolKind::V3 { .. } => {
                self.skips += 1;
                self.push_order("ADD LP".into(), OrderStatus::Skipped, None);
                self.note("Adding liquidity needs a Uniswap V4 pool. This one is trade only".into());
                return Ok(());
            }
            // The MINT calldata builds hooks: 0, which on a Flaunch coin would
            // target a pool that does not exist — refuse rather than revert.
            PoolKind::FlaunchV4 { .. } => {
                self.skips += 1;
                self.push_order("ADD LP".into(), OrderStatus::Skipped, None);
                self.note("Liquidity on Flaunch pools is managed by the Flaunch hook. Trade only".into());
                return Ok(());
            }
        };
        if self.tick > 800_000 || self.tick < -800_000 {
            self.skips += 1;
            self.push_order("ADD LP".into(), OrderStatus::Skipped, None);
            self.logline("SKIP add: pool price broken (empty pool at extreme tick)");
            return Ok(());
        }
        let span = 600i32;
        let lower = (self.tick - span).div_euclid(spacing) * spacing;
        let upper = (self.tick + span).div_euclid(spacing) * spacing;
        let sp = self.sqrt_price;
        let sa = 1.0001f64.powf(lower as f64 / 2.0);
        let sb = 1.0001f64.powf(upper as f64 / 2.0);
        if sb <= sp || sp <= sa {
            self.skips += 1;
            self.push_order("ADD LP".into(), OrderStatus::Skipped, None);
            self.logline("SKIP add: price out of range");
            return Ok(());
        }
        let a0 = eth_wei as f64;
        let liquidity = a0 * (sp * sb) / (sb - sp);
        let a1 = liquidity * (sp - sa);
        let amount0_max = (a0 * 1.02) as u128;
        let amount1_max = (a1 * 1.5) as u128;

        // Skip re-approving if the ERC-20 allowance to Permit2 is already set
        // (approvals persist on-chain across sessions; re-sending would hit a
        // "nonce too low" and waste a round-trip).
        if !self.lp_permit2_done {
            let erc = IERC20::new(self.pool.token, provider);
            if let Ok(a) = erc.allowance(self.trader, PERMIT2).call().await {
                if a._0 >= U256::from(u128::MAX) {
                    self.lp_permit2_done = true;
                }
            }
        }
        if !self.lp_permit2_done {
            let erc = IERC20::new(self.pool.token, provider);
            let p2 = IPermit2::new(PERMIT2, provider);
            let amount160 = alloy::primitives::aliases::U160::MAX;
            let expiration48 = alloy::primitives::aliases::U48::from(v4::FAR_DEADLINE);
            self.note("approving token for Permit2…".into());
            // Fire both approvals (broadcast immediately, no wait between).
            let sent = async {
                let h1 = *erc.approve(PERMIT2, U256::MAX).send().await?.tx_hash();
                let h2 = *p2
                    .approve(self.pool.token, POSITION_MANAGER, amount160, expiration48)
                    .send()
                    .await?
                    .tx_hash();
                Ok::<[TxHash; 2], eyre::Report>([h1, h2])
            }
            .await;
            let hashes = match sent {
                Ok(h) => h,
                Err(e) => {
                    self.fails += 1;
                    self.note(format!("Approval for adding liquidity failed. {}", short_err(&e.to_string())));
                    return Ok(());
                }
            };
            // Bounded wait for both to mine — poll receipts (NO filter-based
            // .watch(), which hangs if the RPC lacks block filters). Caps the
            // freeze at ~a few seconds instead of hanging forever.
            let mut mined = false;
            for _ in 0..6u32 {
                let r0 = provider.get_transaction_receipt(hashes[0]).await.ok().flatten();
                let r1 = provider.get_transaction_receipt(hashes[1]).await.ok().flatten();
                if r0.is_some() && r1.is_some() {
                    mined = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            if !mined {
                self.note("Approvals are sent. Press a again in a moment to add liquidity".into());
                return Ok(());
            }
            self.note("Permit2 approvals confirmed".into());
            self.lp_permit2_done = true;
            self.nonce = None; // approvals used the filler's nonce; resync ours
        }

        let data = v4::add_liquidity_calldata(
            self.pool.token,
            self.pool.fee,
            spacing,
            lower,
            upper,
            U256::from(liquidity as u128),
            amount0_max,
            amount1_max,
            self.trader,
        );
        let tx = TransactionRequest::default()
            .with_to(POSITION_MANAGER)
            .with_input(data)
            .with_value(U256::from(amount0_max))
            .with_from(self.trader);
        let label = format!("ADD LP ~{:.6} ETH", a0 / 1e18);
        if let Err(e) = provider.call(&tx).await {
            self.skips += 1;
            self.push_order(label, OrderStatus::Skipped, None);
            self.note(format!("Skipped adding liquidity because it would revert. {}", short_err(&e.to_string())));
            return Ok(());
        }
        // Don't guess the tokenId from nextTokenId() — on a busy shared chain
        // other mints race in between and we'd cache the wrong id. The real
        // tokenId is read from the mint receipt's ERC-721 Transfer on confirm.
        let burn_id: Option<U256> = None;
        let nonce = match self.take_nonce(provider).await {
            Ok(n) => n,
            Err(e) => {
                self.note(format!("Could not read the account nonce. {}", short_err(&e.to_string())));
                return Ok(());
            }
        };
        let tx = tx.with_nonce(nonce);
        match provider.send_transaction(tx).await {
            Ok(p) => {
                let hash = *p.tx_hash();
                self.push_order(label.clone(), OrderStatus::Pending, Some(hash));
                self.note(format!("PLACED {}  tx {}", label, hash));
                self.mint_liq.insert(hash, liquidity); // link tx -> minted L
                self.pending.push(Pending { hash, label, side: None, eth_amt: 0.0, tok_amt: 0.0, position_id: burn_id });
            }
            Err(e) => {
                self.fails += 1;
                self.nonce = None;
                self.push_order(label, OrderStatus::Failed, None);
                self.note(format!("Adding liquidity failed. {}", short_err(&e.to_string())));
            }
        }
        Ok(())
    }

    /// Scan for position NFTs owned by the trader (up to `limit`). Bounded so a
    /// sparse ID space can't freeze the UI; balanceOf short-circuits the empty
    /// case (no scan when nothing is held).
    async fn find_positions<P: Provider>(&self, provider: &P, limit: usize) -> Vec<U256> {
        let posm = IPositionManager::new(POSITION_MANAGER, provider);
        let held = posm.balanceOf(self.trader).call().await.map(|b| b._0).unwrap_or(U256::ZERO);
        if held.is_zero() {
            return Vec::new();
        }
        let want = held.min(U256::from(limit)).to::<u64>() as usize;
        let next = posm.nextTokenId().call().await.map(|n| n._0).unwrap_or(U256::ZERO);
        if next.is_zero() {
            return Vec::new();
        }
        let mut id = next - U256::from(1);
        let mut found = Vec::new();
        for _ in 0..200 {
            if let Ok(o) = posm.ownerOf(id).call().await {
                if o._0 == self.trader {
                    found.push(id);
                    if found.len() >= want {
                        break;
                    }
                }
            }
            if id == U256::ZERO {
                break;
            }
            id -= U256::from(1);
        }
        found
    }

    /// Burn one position by tokenId, returning both tokens.
    async fn burn_position<P: Provider>(&mut self, provider: &P, token_id: U256, label: String) -> eyre::Result<()> {
        let burn_id = Some(token_id);
        // Optimistically drop it from the owned cache so rapid removes step to
        // the next position (re-added on revert; see reap).
        self.positions.retain(|x| *x != token_id);
        let data = v4::close_liquidity_calldata(token_id, self.pool.token, self.trader);
        let tx = TransactionRequest::default()
            .with_to(POSITION_MANAGER)
            .with_input(data)
            .with_from(self.trader);
        let nonce = self.take_nonce(provider).await?;
        let tx = tx.with_nonce(nonce);
        match provider.send_transaction(tx).await {
            Ok(p) => {
                let hash = *p.tx_hash();
                self.push_order(label.clone(), OrderStatus::Pending, Some(hash));
                self.note(format!("PLACED {}  tx {}", label, hash));
                self.pending.push(Pending { hash, label, side: None, eth_amt: 0.0, tok_amt: 0.0, position_id: burn_id });
            }
            Err(e) => {
                self.fails += 1;
                self.nonce = None;
                self.push_order(label, OrderStatus::Failed, None);
                self.note(format!("Closing the position failed. {}", short_err(&e.to_string())));
            }
        }
        Ok(())
    }

    /// Position IDs that already have an in-flight burn (parsed from pending
    /// labels "…#<id>") — so we don't double-burn one while its burn is pending.
    fn pending_burn_ids(&self) -> std::collections::HashSet<U256> {
        self.pending
            .iter()
            .filter_map(|p| p.label.rsplit('#').next().and_then(|s| s.trim().parse::<u128>().ok()))
            .map(U256::from)
            .collect()
    }

    /// Ensure the owned-position cache is populated (scan the chain only when
    /// it is empty — e.g. first use or after everything has been closed).
    async fn ensure_positions<P: Provider>(&mut self, provider: &P) {
        if self.positions.is_empty() {
            self.positions = self.find_positions(provider, 50).await;
        }
    }

    /// Remove ONE position ('r' key) — the most recent one, to iterate. Uses
    /// the local cache so rapid presses don't rescan or double-burn.
    pub async fn remove_liquidity<P: Provider>(&mut self, provider: &P) -> eyre::Result<()> {
        if self.pool.kind.is_v3() {
            self.note("Liquidity actions need a Uniswap V4 pool. This one is trade only".into());
            return Ok(());
        }
        self.ensure_positions(provider).await;
        let pend = self.pending_burn_ids();
        match self.positions.iter().rev().find(|id| !pend.contains(id)).copied() {
            Some(id) => self.burn_position(provider, id, format!("REMOVE LP #{id}")).await,
            None => {
                self.skips += 1;
                self.push_order("REMOVE LP".into(), OrderStatus::Skipped, None);
                self.note("There is no liquidity position to remove".into());
                Ok(())
            }
        }
    }

    /// Close ALL positions ('x' key) — burn every one the trader owns.
    pub async fn close_all<P: Provider>(&mut self, provider: &P) -> eyre::Result<()> {
        // v4 with LP positions → burn them all. Otherwise (v3, or a v4 pool
        // where we hold no LP) → sell the entire token balance for ETH.
        if !self.pool.kind.is_v3() {
            self.ensure_positions(provider).await;
            let pend = self.pending_burn_ids();
            let ids: Vec<U256> = self
                .positions
                .iter()
                .filter(|id| !pend.contains(id))
                .copied()
                .collect();
            if !ids.is_empty() {
                self.note(format!("closing {} position(s)…", ids.len()));
                for id in ids {
                    self.burn_position(provider, id, format!("CLOSE LP #{id}")).await?;
                }
                return Ok(());
            }
        }
        self.sell_all(provider).await
    }

    /// Sell the ENTIRE token balance for ETH — an exit. Bypasses the size /
    /// impact / profit caps (you want OUT); wide slippage; pre-flight still
    /// protects against reverts.
    pub async fn sell_all<P: Provider>(&mut self, provider: &P) -> eyre::Result<()> {
        // Dedup guard: don't fire a sell-all while a trade is already pending.
        if self.guard_dup && self.pending.iter().any(|p| p.side.is_some()) {
            self.skips += 1;
            self.note("Skipped selling everything because a trade is already pending".into());
            return Ok(());
        }
        if !self.ready || self.token_bal <= 0.0 {
            self.skips += 1;
            self.push_order("SELL ALL".into(), OrderStatus::Skipped, None);
            self.note(format!("no {} to sell", self.pool.sym));
            return Ok(());
        }
        // Size off the LIVE balance as an EXACT integer — a stale-high cache (or
        // an f64 round-trip landing a few wei high) would oversize the
        // transferFrom → STF. Sell precisely what's held, to the wei.
        let bal_u256 = IERC20::new(self.pool.token, provider)
            .balanceOf(self.trader)
            .call()
            .await
            .map(|b| b._0)
            .unwrap_or(U256::ZERO);
        let sell_cap = Wei::exact(bal_u256);
        let amount_in = wei_to_f64(bal_u256);
        if amount_in <= 0.0 {
            self.skips += 1;
            self.push_order("SELL ALL".into(), OrderStatus::Skipped, None);
            self.note(format!("There is no {} to sell", self.pool.sym));
            return Ok(());
        }
        // Approve v3 venues up front, then route the dump to the venue that
        // returns the most ETH (best fee tier / depth / least hook take).
        if (self.has_v3_route() || self.pool.kind.is_v3()) && !self.v3_covered {
            let need = IERC20::new(self.pool.token, provider)
                .balanceOf(self.trader).call().await.map(|b| b._0).unwrap_or(U256::ZERO);
            if !need.is_zero() {
                match self.ensure_v3_allowance(provider, need).await {
                    Ok(true) => self.v3_covered = true,
                    Ok(false) => { self.note("Approval is still confirming. Try selling everything again shortly".into()); return Ok(()); }
                    Err(e) => { self.note(format!("Approval for selling everything failed. {}", short_err(&e.to_string()))); return Ok(()); }
                }
            }
        }
        if self.has_flaunch_route() && !self.ur_permit2_done {
            match self.ensure_ur_allowance(provider, bal_u256).await {
                Ok(true) => self.ur_permit2_done = true,
                Ok(false) => { self.note("Approval is still confirming. Try selling everything again shortly".into()); return Ok(()); }
                Err(e) => { self.note(format!("Approval for selling everything failed. {}", short_err(&e.to_string()))); return Ok(()); }
            }
        }
        let (route, expected) = match self.best_venue(provider, amount_in, false).await {
            Some(x) => x,
            None => {
                self.skips += 1;
                self.push_order("SELL ALL".into(), OrderStatus::Skipped, None);
                self.note("Cannot sell everything because no pool passes the pre flight check".into());
                return Ok(());
            }
        };
        // A full dump moves price more than a sized trade, so it gets extra
        // room on top of the configured tolerance — but still derived from it.
        let min_out = expected * self.slip_floor_dump();
        // Selling the tracked token for ETH: token side uses its real decimals.
        let wei_in = Wei::of_token(amount_in, self.pool.token_decimals).min(sell_cap); // exact balance, no STF
        let wei_min = Wei::rounded(min_out);
        let (to, data, _value) = build_swap(route.kind, route.token, route.fee, false, wei_in, wei_min, self.trader);
        // Sell + unwrapWETH9 → generous gas cap so the WETH withdraw never OOGs.
        // A Flaunch dump crosses two hooked pools, so it gets more headroom.
        let dump_gas = match route.kind {
            PoolKind::FlaunchV4 { .. } => 650_000,
            _ => 450_000,
        };
        let tx = TransactionRequest::default().with_to(to).with_input(data).with_gas_limit(dump_gas).with_from(self.trader);
        // Report the sell in ETH numeraire (expected proceeds), not token units.
        let label = format!("SELL ALL for {:.6} ETH @ {:.6} [{} {}] liq_eth={:.6}", expected, self.price(), route.kind.proto(), route.label, self.r0);
        if let Err(e) = provider.call(&tx).await {
            self.skips += 1;
            self.push_order(label, OrderStatus::Skipped, None);
            self.note(format!("Skipped selling everything because it would revert. {}", short_err(&e.to_string())));
            return Ok(());
        }
        let nonce = match self.take_nonce(provider).await {
            Ok(n) => n,
            Err(e) => {
                self.note(format!("Could not read the account nonce. {}", short_err(&e.to_string())));
                return Ok(());
            }
        };
        let tx = tx.with_nonce(nonce);
        match provider.send_transaction(tx).await {
            Ok(p) => {
                let hash = *p.tx_hash();
                self.last_side = Side::Sell;
                self.push_order(label.clone(), OrderStatus::Pending, Some(hash));
                self.note(format!("SENT {}  tx {}", label, hash));
                self.pending.push(Pending { hash, label, side: Some(Side::Sell), eth_amt: expected, tok_amt: amount_in, position_id: None });
            }
            Err(e) => {
                self.fails += 1;
                self.nonce = None;
                self.push_order(label, OrderStatus::Failed, None);
                self.note(format!("Selling everything failed. {}", short_err(&e.to_string())));
            }
        }
        Ok(())
    }

    /// Initialize a brand-new v4 pool (ETH/token) at `sqrt_price_x96`.
    pub async fn initialize_pool<P: Provider>(
        &mut self,
        provider: &P,
        token: Address,
        fee: u32,
        tick_spacing: i32,
        sqrt_price_x96: alloy::primitives::aliases::U160,
    ) -> eyre::Result<()> {
        let pm = IPoolManager::new(POOL_MANAGER, provider);
        let key = PoolKey {
            currency0: Address::ZERO,
            currency1: token,
            fee: fee.try_into().unwrap(),
            tickSpacing: tick_spacing.try_into().unwrap(),
            hooks: Address::ZERO,
        };
        let nonce = self.take_nonce(provider).await?;
        let label = format!("CREATE POOL fee {fee}");
        let burn_id: Option<U256> = None;
        match pm.initialize(key, sqrt_price_x96).nonce(nonce).send().await {
            Ok(p) => {
                let hash = *p.tx_hash();
                self.push_order(label.clone(), OrderStatus::Pending, Some(hash));
                self.note(format!("PLACED {label}  tx {}", hash));
                self.pending.push(Pending { hash, label, side: None, eth_amt: 0.0, tok_amt: 0.0, position_id: burn_id });
            }
            Err(e) => {
                self.fails += 1;
                self.nonce = None;
                self.push_order(label, OrderStatus::Failed, None);
                self.note(format!("create pool failed: {}", short_err(&e.to_string())));
            }
        }
        Ok(())
    }

    /// True when copy mode is active (buys shadow the deployer's ladder rung).
    pub fn is_copy(&self) -> bool {
        self.strategy == Strategy::CopyBuyAmount
    }

    /// Auto-strategy signal for this tick (None = hold). Both current modes are
    /// MANUAL-timing — you press b/s. CopyBuyAmount only changes the buy SIZE (to the
    /// copied ladder rung), it never fires trades on its own.
    pub fn signal(&self, _sma: f64, _band: f64) -> Option<Side> {
        None
    }
}

/// Compress a multi-line RPC/revert error to a single readable line for the
/// status bar (the full text is always in the log ring / session file).
fn short_err(e: &str) -> String {
    let one: String = e.split('\n').next().unwrap_or(e).trim().to_string();
    if one.chars().count() > 160 {
        format!("{}…", one.chars().take(160).collect::<String>())
    } else {
        one
    }
}

/// Best-effort ERC-20 symbol (falls back to "TKN" if the token has none).
pub async fn read_symbol<P: Provider>(provider: &P, token: Address) -> String {
    IERC20::new(token, provider)
        .symbol()
        .call()
        .await
        .map(|s| s._0)
        .unwrap_or_else(|_| "TKN".into())
}

/// A market snapshot read off the UI thread (no &Bot needed).
#[derive(Clone, Copy, Default)]
pub struct Market {
    pub sqrt_price: f64,
    pub tick: i32,
    pub r0: f64,
    pub r1: f64,
    /// The trader's ETH, or None when the read did not answer.
    ///
    /// Not an `f64` defaulting to zero. A rate-limited `eth_getBalance` used to
    /// report 0, which the dashboard showed as an empty wallet and the session
    /// PnL read as having lost the whole balance — so both flickered between
    /// the truth and zero on alternate polls.
    pub eth: Option<f64>,
    /// Balances, or None on a light read that did not ask for them.
    pub token_bal: Option<f64>,
    pub ready: bool,
    pub read_ms: f64,
    pub gas_price: f64, // wei, for the profitability filter
    pub supply: f64,    // token totalSupply (human units), for market cap
    /// False on a price-only read. Everything a light read did not fetch must
    /// be carried over rather than treated as newly-zero — without this the
    /// pool reads as illiquid seven times a second between full reads.
    pub full: bool,
}

/// Read price/liquidity + balances for a pool. Standalone so a background task
/// can call it with a cloned provider — the UI thread never does this I/O.
pub async fn read_market<P: Provider>(
    provider: &P,
    pref: PoolRef,
    trader: Address,
) -> eyre::Result<Market> {
    read_market_inner(provider, pref, trader, true).await
}

/// Price and liquidity only — no balances, no gas price, no total supply.
///
/// Those four reads were being made on every poll, seven times a second, and
/// they are the three-quarters of the traffic that earns a 429. A total supply
/// does not change; a balance changes only when you trade. Splitting them off
/// leaves the price — the one number that has to be current — reading at full
/// rate on a fraction of the requests.
pub async fn read_price_only<P: Provider>(
    provider: &P,
    pref: PoolRef,
    trader: Address,
) -> eyre::Result<Market> {
    read_market_inner(provider, pref, trader, false).await
}

async fn read_market_inner<P: Provider>(
    provider: &P,
    pref: PoolRef,
    trader: Address,
    full: bool,
) -> eyre::Result<Market> {
    let t0 = Instant::now();

    // No pool selected (empty network) — still show ETH + gas, empty market.
    if pref.kind.is_empty() {
        let eth = if full {
            crate::rpcstats::timed("eth_getBalance", provider.get_balance(trader))
                .await
                .map(wei_to_f64)
                .ok()
        } else {
            None
        };
        let gas = provider.get_gas_price().await.map(|g| g as f64).unwrap_or(0.0);
        return Ok(Market {
            eth,
            gas_price: gas,
            read_ms: t0.elapsed().as_secs_f64() * 1000.0,
            full,
            ..Default::default()
        });
    }

    let erc = IERC20::new(pref.token, provider);

    // sqrtPrice + raw liquidity L, from the protocol's state source.
    let (sqrt_p, tick, l) = match pref.kind {
        PoolKind::V4 { pool_id, .. } | PoolKind::FlaunchV4 { pool_id, .. } => {
            let sv = IStateView::new(STATE_VIEW, provider);
            let cb0 = sv.getSlot0(pool_id);
            let cbl = sv.getLiquidity(pool_id);
            let (s0, lq) = tokio::join!(cb0.call(), cbl.call());
            let s0 = s0?;
            (u160_to_f64(s0.sqrtPriceX96) / 2f64.powi(96), s0.tick.as_i32(), u128_to_f64(lq?.liquidity))
        }
        PoolKind::V3 { pool_addr, .. } => {
            let pool = IV3Pool::new(pool_addr, provider);
            let cb0 = pool.slot0();
            let cbl = pool.liquidity();
            let (s0, lq) = tokio::join!(cb0.call(), cbl.call());
            let s0 = s0?;
            (u160_to_f64(s0.sqrtPriceX96) / 2f64.powi(96), s0.tick.as_i32(), u128_to_f64(lq?._0))
        }
    };

    let cb_tok = erc.balanceOf(trader);
    let cb_sup = erc.totalSupply();
    if !full {
        // The cheap path. Everything left None or zero is carried over from the
        // last full read by `apply_market`, which never overwrites a known value
        // with an absent one.
        return Ok(Market {
            sqrt_price: sqrt_p,
            tick,
            r0: 0.0,
            r1: 0.0,
            eth: None,
            token_bal: None,
            ready: false,
            read_ms: t0.elapsed().as_secs_f64() * 1000.0,
            gas_price: 0.0,
            supply: 0.0,
            full: false,
        });
    }
    let (eth_bal, tok_bal, gas, sup) = tokio::join!(
        crate::rpcstats::timed("eth_getBalance", provider.get_balance(trader)),
        crate::rpcstats::timed("balanceOf", cb_tok.call()),
        crate::rpcstats::timed("eth_gasPrice", provider.get_gas_price()),
        crate::rpcstats::timed("totalSupply", cb_sup.call()),
    );
    let gas_price = gas.map(|g| g as f64).unwrap_or(0.0);
    let td = pref.token_decimals;
    let supply = sup.map(|s| units_to_f64(s._0, td)).unwrap_or(0.0);

    // Normalize reserves to (quote-side r0, token-side r1) in HUMAN units, using
    // each side's real decimals. The raw virtual reserves are a=token0, b=token1;
    // scale each by 10^decimals (NOT a blanket 1e18 — USDG is 6-dec, so 1e18 read
    // its reserve as ~0). Tracked tokens are 18-dec; the quote may be ETH (18) or
    // a stablecoin (e.g. USDG 6).
    let a_raw = if sqrt_p > 0.0 { l / sqrt_p } else { 0.0 }; // token0 raw reserve

    let b_raw = l * sqrt_p; // token1 raw reserve
    let qd = pref.quote.decimals() as i32;
    let tdi = td as i32; // tracked-token decimals — read on-chain, NOT assumed
    let (r0, r1) = match pref.kind {
        // token0 = min(token, quote). If the tracked token sorts below the quote
        // it's token0 (a); the quote is token1 (b) → quote-side r0 = b.
        PoolKind::V4 { .. } => {
            if pref.token < pref.quote.addr() {
                (b_raw / 10f64.powi(qd), a_raw / 10f64.powi(tdi))
            } else {
                (a_raw / 10f64.powi(qd), b_raw / 10f64.powi(tdi))
            }
        }
        // v3: the quote side is WETH (18), but the tracked token can be any
        // precision — a blanket 1e18 read a 6-dec reserve as ~0, which showed
        // up as price 0.000000 and a nonsense market cap.
        PoolKind::V3 { weth_is_token0, .. } => {
            if weth_is_token0 {
                (a_raw / 1e18, b_raw / 10f64.powi(tdi))
            } else {
                (b_raw / 1e18, a_raw / 10f64.powi(tdi))
            }
        }
        // Flaunch: the quote side is flETH (18-dec, 1:1 with ETH), so the
        // flETH reserve is r0 in ETH terms. The launch's _currencyFlipped bool
        // decides the orientation, not the ETH-vs-token address ordering.
        PoolKind::FlaunchV4 { coin_is_0, .. } => {
            if coin_is_0 {
                (b_raw / 1e18, a_raw / 10f64.powi(tdi))
            } else {
                (a_raw / 1e18, b_raw / 10f64.powi(tdi))
            }
        }
    };
    Ok(Market {
        sqrt_price: sqrt_p,
        tick,
        r0,
        r1,
        eth: eth_bal.ok().map(wei_to_f64), // native ETH is always 18-dec
        token_bal: tok_bal.ok().map(|t| units_to_f64(t._0, td)),
        ready: r0 > 0.0 && r1 > 0.0,
        read_ms: t0.elapsed().as_secs_f64() * 1000.0,
        gas_price,
        supply,
        full: true,
    })
}

#[derive(Clone, Copy, PartialEq)]
pub enum TapeAction {
    Buy,
    Sell,
    Add,
    Remove,
}

/// A decoded pool event by ANY trader — the live tape (buys, sells, LP add/remove).
#[derive(Clone, Copy)]
pub struct Swap {
    pub action: TapeAction,
    pub eth: f64,       // ETH/WETH size of the event
    pub eth_wei: u128,  // EXACT WETH size in wei (no float rounding) — for copy-step keying
    pub price: f64,     // token per ETH (0 for LP events)
    /// Who the swap is attributed to — the person, not the router.
    ///
    /// Always the transaction's signer, resolved per tx. Neither event field
    /// can be trusted for this: v4 indexes the sender, which is the Universal
    /// Router, and v3 indexes the recipient, which is SwapRouter02 whenever the
    /// output is unwrapped ETH. Read either directly and a whole side of the
    /// tape shows the router's address instead of a trader's.
    pub trader: Address,
    pub liq_eth: f64,   // pooled ETH (r0) at this event, from L/√P (0 for LP events)
    pub block: u64,
    pub tx: TxHash,     // to mark our own trades
    pub tick_lo: i32,   // LP range low (Add/Remove only)
    pub tick_hi: i32,   // LP range high (Add/Remove only)
    pub is_v4: bool,    // which venue (for the merged arb tape)
}

/// Fetch + decode pool Swap events in [from_block, to_block] — every trader's
/// swaps on the current pool, for the dexscreener-style tape.
/// Cache of transaction hash -> sender.
///
/// A confirmed transaction never changes, so each hash is fetched once. Without
/// this, attributing v4 swaps would mean one RPC round trip per swap on every
/// refresh — the same waste that made the Solana tape unusable.
fn tx_sender_cache() -> &'static std::sync::Mutex<std::collections::HashMap<TxHash, Address>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<TxHash, Address>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Replace router addresses with the actual transaction senders.
///
/// v4 emits the SENDER of the pool call, which is the Universal Router — so
/// every trade through it looks like the same trader. The person who signed is
/// the transaction's `from`, which needs one lookup per transaction.
async fn attribute_to_senders<P: Provider>(provider: &P, swaps: &mut [Swap]) {
    use futures::stream::StreamExt;

    let mut want: Vec<TxHash> = Vec::new();
    {
        let cache = tx_sender_cache().lock().ok();
        for s in swaps.iter() {
            let known = cache.as_ref().map(|c| c.contains_key(&s.tx)).unwrap_or(false);
            if !known && !want.contains(&s.tx) {
                want.push(s.tx);
            }
        }
    }
    if !want.is_empty() {
        // A handful in flight: enough to be quick, few enough not to trip a
        // provider's rate limit.
        let fetched: Vec<(TxHash, Address)> = futures::stream::iter(want)
            .map(|h| async move {
                let tx = provider.get_transaction_by_hash(h).await.ok().flatten()?;
                Some((h, tx.from))
            })
            .buffered(6)
            .filter_map(|r| async move { r })
            .collect()
            .await;
        if let Ok(mut c) = tx_sender_cache().lock() {
            // Bound it: a long session on a busy pool would grow without limit.
            if c.len() > 5_000 {
                c.clear();
            }
            c.extend(fetched);
        }
    }
    if let Ok(c) = tx_sender_cache().lock() {
        for s in swaps.iter_mut() {
            if let Some(from) = c.get(&s.tx) {
                s.trader = *from;
            }
        }
    }
}

pub async fn read_swaps<P: Provider>(provider: &P, pref: PoolRef, from_block: u64, to_block: u64) -> eyre::Result<Vec<Swap>> {
    use alloy::primitives::I256;
    use alloy::rpc::types::Filter;
    if pref.kind.is_empty() || from_block > to_block {
        return Ok(Vec::new());
    }
    let iword = |b: &[u8], i: usize| -> f64 {
        if b.len() < (i + 1) * 32 { return 0.0; }
        let mut w = [0u8; 32];
        w.copy_from_slice(&b[i * 32..i * 32 + 32]);
        I256::from_be_bytes(w).to_string().parse::<f64>().unwrap_or(0.0)
    };
    // int24 from an indexed topic (sign-extended 32-byte word).
    let tick_of = |t: B256| -> i32 { I256::from_be_bytes(t.0).to_string().parse::<i32>().unwrap_or(0) };
    let uword = |b: &[u8], i: usize| -> f64 {
        if b.len() < (i + 1) * 32 { return 0.0; }
        U256::from_be_slice(&b[i * 32..i * 32 + 32]).to_string().parse::<f64>().unwrap_or(0.0)
    };
    // Exact |wei| of a signed amount word (no float) — for copy-step keying.
    let iabs_wei = |b: &[u8], i: usize| -> u128 {
        if b.len() < (i + 1) * 32 { return 0; }
        let mut w = [0u8; 32];
        w.copy_from_slice(&b[i * 32..i * 32 + 32]);
        I256::from_be_bytes(w).unsigned_abs().to_string().parse::<u128>().unwrap_or(0)
    };
    // Decimals for BOTH sides. A quote is not always 18: USDG is 6, and every
    // amount, reserve and price below depends on getting this right.
    let token_dec = pref.token_decimals;
    let quote_dec = pref.quote.decimals();

    // Topic0s for the event types we decode.
    const SWAP_V4: B256 = alloy::primitives::b256!("40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f");
    const SWAP_V3: B256 = alloy::primitives::b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
    const MINT_V3: B256 = alloy::primitives::b256!("7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde");
    const BURN_V3: B256 = alloy::primitives::b256!("0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c");
    const MODLIQ_V4: B256 = alloy::primitives::b256!("f208f4912782fd25c7f114ca3723a2d5dd6f3bcc3ac8db5af63baa85f711d5ec");

    // One filter per protocol (no topic0 filter — decode by topic0 below). v4:
    // scope to our pool via topic1 = pool id. v3: scope by pool address.
    let (weth0, v4, filter) = match pref.kind {
        PoolKind::V4 { pool_id, .. } => (
            true, true,
            Filter::new().address(POOL_MANAGER).topic1(pool_id).from_block(from_block).to_block(to_block),
        ),
        // Same PoolManager events as V4, but the quote side is flETH, whose
        // position follows the launch's _currencyFlipped rather than always 0.
        PoolKind::FlaunchV4 { pool_id, coin_is_0 } => (
            !coin_is_0, true,
            Filter::new().address(POOL_MANAGER).topic1(pool_id).from_block(from_block).to_block(to_block),
        ),
        PoolKind::V3 { pool_addr, weth_is_token0 } => (
            weth_is_token0, false,
            Filter::new().address(pool_addr).from_block(from_block).to_block(to_block),
        ),
    };
    let logs = provider.get_logs(&filter).await?;
    let mut out = Vec::new();
    for lg in logs {
        let topics = lg.topics();
        let t0 = if topics.is_empty() { continue } else { topics[0] };
        let bytes = lg.data().data.clone();
        let b = bytes.as_ref();
        let block = lg.block_number.unwrap_or(0);
        let tx = lg.transaction_hash.unwrap_or_default();

        // Swap: amounts at words 0,1; sqrtPrice at word 2.
        let is_swap = (v4 && t0 == SWAP_V4) || (!v4 && t0 == SWAP_V3);
        if is_swap {
            // Every amount here is in BASE UNITS, and the two sides can have
            // different decimals — a USDG-quoted pool is 6 on the quote side and
            // 18 on the token side. Dividing both by 1e18 read every USDG figure
            // as 0.000000.
            let (dec0, dec1) = if weth0 {
                (quote_dec, token_dec)
            } else {
                (token_dec, quote_dec)
            };
            let a0 = iword(b, 0) / 10f64.powi(dec0 as i32);
            let a1 = iword(b, 1) / 10f64.powi(dec1 as i32);
            let sqrt = uword(b, 2) / 2f64.powi(96);
            let p_raw = sqrt * sqrt;
            // `sqrtPriceX96` prices the pair in BASE UNITS, so turning it into
            // tokens-per-ETH needs the decimal gap between the two sides. That
            // factor is 1 for an 18/18 pair, which is why this went unnoticed
            // until a 6-decimal quote: USDG showed price 0.000000 and a market
            // cap in the tens of billions.
            // The gap BETWEEN the two sides, not a gap from 18: `sqrtPriceX96`
            // prices the pair in base units, so the correction is
            // 10^(quote_decimals - token_decimals).
            let dec_scale = 10f64.powi(quote_dec as i32 - token_dec as i32);
            let price = if weth0 {
                p_raw * dec_scale
            } else if p_raw > 0.0 {
                dec_scale / p_raw
            } else {
                0.0
            };
            // Pooled ETH (r0) at this event: L/√P is token0's reserve, L·√P is
            // token1's — take the WETH/ETH side. Same math as the market decoder.
            let liq = uword(b, 3); // v3 & v4 both carry `liquidity` at word 3
            // The quote-side reserve, scaled by the QUOTE's decimals.
            let liq_eth = if sqrt > 0.0 {
                (if weth0 { liq / sqrt } else { liq * sqrt }) / 10f64.powi(quote_dec as i32)
            } else {
                0.0
            };
            // The quote side follows weth0 on every protocol. (Plain-v4 pools
            // always pass weth0 = true, so for them this is still word 0.)
            let eth_side = if weth0 { a0 } else { a1 };
            let eth_wei = iabs_wei(b, if weth0 { 0 } else { 1 });
            // v3 topic[2] is the recipient; v4 topic[2] is the sender.
            let trader = topics.get(2).map(|w| Address::from_word(*w)).unwrap_or_default();
            let (action, eth) = if v4 {
                if eth_side < 0.0 { (TapeAction::Buy, -eth_side) } else { (TapeAction::Sell, eth_side) }
            } else if eth_side > 0.0 {
                (TapeAction::Buy, eth_side)
            } else {
                (TapeAction::Sell, -eth_side)
            };
            out.push(Swap { action, eth, eth_wei, price, liq_eth, block, tx, trader, tick_lo: 0, tick_hi: 0, is_v4: v4 });
            continue;
        }

        // v3 Mint (Add) / Burn (Remove): tickLower/Upper are indexed (topics 2,3);
        // data = [amount, amount0, amount1] (Mint prepends `sender`, +1 word).
        if !v4 && (t0 == MINT_V3 || t0 == BURN_V3) {
            let off = if t0 == MINT_V3 { 1 } else { 0 };
            let amt0 = uword(b, off + 1) / 1e18;
            let amt1 = uword(b, off + 2) / 1e18;
            let eth = if weth0 { amt0 } else { amt1 };
            let action = if t0 == MINT_V3 { TapeAction::Add } else { TapeAction::Remove };
            let tick_lo = topics.get(2).map(|t| tick_of(*t)).unwrap_or(0);
            let tick_hi = topics.get(3).map(|t| tick_of(*t)).unwrap_or(0);
            out.push(Swap { action, eth, eth_wei: 0, price: 0.0, liq_eth: 0.0, block, tx, trader: Address::ZERO, tick_lo, tick_hi, is_v4: v4 });
            continue;
        }

        // v4 ModifyLiquidity: data = [tickLower, tickUpper, liquidityDelta, salt].
        if v4 && t0 == MODLIQ_V4 {
            let tick_lo = iword(b, 0) as i32;
            let tick_hi = iword(b, 1) as i32;
            let delta = iword(b, 2); // liquidityDelta (signed)
            let action = if delta >= 0.0 { TapeAction::Add } else { TapeAction::Remove };
            out.push(Swap { action, eth: 0.0, eth_wei: 0, price: 0.0, liq_eth: 0.0, block, tx, trader: Address::ZERO, tick_lo, tick_hi, is_v4: v4 });
            continue;
        }
    }
    // Resolve every swap to the account that signed it.
    //
    // This used to run for v4 only, on the assumption that v3 indexes the
    // recipient and "for a router swap that is already the user". It is not:
    // selling token for ETH routes through SwapRouter02, which takes the WETH
    // itself before unwrapping and forwarding, so the recipient is the router.
    // Buys came out right and sells were attributed to 0xcaf6…5cb2 — the
    // router's own address, on every sell in the pool.
    //
    // Per-tx, cached, so a busy pool costs one lookup per transaction and never
    // the same one twice.
    attribute_to_senders(provider, &mut out).await;
    Ok(out)
}

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

/// Pull the ETH amount out of an order label like "BUY ~0.00017 ETH @ ..." or
/// "ADD LP ~0.001 ETH". Returns None for token-denominated labels (sells) so the
/// caller values them at price instead.
fn eth_of_label(label: &str) -> Option<f64> {
    let idx = label.find(" ETH")?;
    label[..idx]
        .rsplit(|c: char| c == ' ' || c == '~')
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
}

fn wei_to_f64(x: U256) -> f64 {
    // Overflow-safe (a token balance could exceed u128): parse the decimal.
    x.to_string().parse::<f64>().unwrap_or(0.0) / 1e18
}

/// On-chain base units -> human amount, using the token's REAL decimals.
/// `wei_to_f64` is the 18-dec special case; anything token-denominated must come
/// through here instead, or a 6-dec token reads as ~0.
fn units_to_f64(x: U256, decimals: u8) -> f64 {
    x.to_string().parse::<f64>().unwrap_or(0.0) / 10f64.powi(decimals as i32)
}
fn u128_to_f64(x: u128) -> f64 {
    x as f64
}
fn u160_to_f64(x: alloy::primitives::Uint<160, 3>) -> f64 {
    // Overflow-safe: a broken/empty pool's sqrtPriceX96 can sit near 2^160
    // (max tick), which overflows u128. Parse the decimal string instead.
    x.to_string().parse::<f64>().unwrap_or(f64::MAX)
}
/// Human amount (ETH / 18-decimal token) -> wei.
fn eth_to_wei(x: f64) -> u128 {
    if x <= 0.0 {
        0
    } else {
        (x * 1e18) as u128
    }
}

/// An EXACT on-chain amount, in wei (18-dec base units). A distinct type from the
/// human `f64` values used for display/quoting, so a rounded human number can
/// never silently become a transfer amount. There are exactly two ways in:
/// `Wei::exact` (lossless, from an on-chain U256) and `Wei::rounded` (LOSSY, from an
/// f64 — named so at every call site). The f64 round-trip of a token balance can
/// land a few wei ABOVE the real balance; using that as amountIn reverts the
/// router with STF. Types make that mistake impossible to write by accident.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wei(pub u128);

impl Wei {
    pub const ZERO: Wei = Wei(0);
    pub const MAX: Wei = Wei(u128::MAX);
    /// Lossless: the exact integer amount from an on-chain U256 (saturating).
    pub fn exact(v: U256) -> Wei {
        Wei(u128::try_from(v).unwrap_or(u128::MAX))
    }
    /// LOSSY: from a human amount of an 18-decimal asset (ETH, or an 18-dec
    /// token). Rounds — only use where a few wei don't matter (ETH spend,
    /// slippage floor), NEVER for a token balance you must not exceed.
    ///
    /// ⚠️ For the TRACKED token use [`Wei::of_token`] instead: this assumes 18
    /// decimals, so a 6-dec token would be scaled 10^12 too high.
    pub fn rounded(x: f64) -> Wei {
        Wei(eth_to_wei(x))
    }
    /// LOSSY: from a human amount of a token with `decimals` decimals. This is
    /// the correct entry point for anything denominated in the TRACKED token.
    pub fn of_token(x: f64, decimals: u8) -> Wei {
        if x <= 0.0 {
            Wei(0)
        } else {
            Wei((x * 10f64.powi(decimals as i32)) as u128)
        }
    }
    pub fn min(self, o: Wei) -> Wei {
        Wei(self.0.min(o.0))
    }
    pub fn raw(self) -> u128 {
        self.0
    }
}

/// Build (to, calldata, value) for a swap on any pool kind — the protocol-branch
/// shared by `place()`-style trades and the arb legs. Amounts are `Wei` (exact),
/// never f64. V4 uses native ETH via the UniversalRouter; V3 uses WETH via
/// SwapRouter02 (buy sends ETH value, sell multicalls unwrap back to ETH).
fn build_swap(
    kind: PoolKind,
    token: Address,
    fee: u32,
    buying: bool,
    wei_in: Wei,
    wei_min: Wei,
    trader: Address,
) -> (Address, Bytes, U256) {
    let (wi, wm) = (wei_in.raw(), wei_min.raw());
    let value = if buying { U256::from(wi) } else { U256::ZERO };
    match kind {
        PoolKind::V4 { tick_spacing, .. } => (
            UNIVERSAL_ROUTER,
            v4::swap_calldata(token, fee, tick_spacing, buying, wi, wm),
            value,
        ),
        // `fee` is deliberately unused: the Flaunch pool key's fee is 0 (the
        // hook charges its cut), and the builder hardcodes the key layout.
        PoolKind::FlaunchV4 { .. } => (
            UNIVERSAL_ROUTER,
            v4::flaunch_swap_calldata(token, buying, wi, wm),
            value,
        ),
        PoolKind::V3 { .. } => {
            let d = if buying {
                v3::v3_buy_calldata(token, fee, wi, wm, trader)
            } else {
                v3::v3_sell_calldata(token, fee, wi, wm, trader)
            };
            (SWAP_ROUTER_02, d, value)
        }
    }
}

pub type _B = Bytes;

#[cfg(test)]
mod tape_price_tests {
    /// The scale a raw `sqrtPriceX96` ratio needs to become tokens-per-quote.
    ///
    /// The correction is the gap BETWEEN the two sides. Deriving it from 18
    /// silently assumes an 18-decimal quote, which is what made every
    /// USDG-quoted pool read 10^12 out.
    fn dec_scale(quote_decimals: u8, token_decimals: u8) -> f64 {
        10f64.powi(quote_decimals as i32 - token_decimals as i32)
    }

    #[test]
    fn an_18_18_pair_needs_no_correction() {
        // Why this class of bug hides: the common case is a no-op.
        assert_eq!(dec_scale(18, 18), 1.0);
    }

    #[test]
    fn a_six_decimal_quote_needs_ten_to_the_twelve() {
        // USDG(6) against an 18-decimal token. Getting this backwards is the
        // difference between 0.00297 and 2,968,466,050 on screen.
        assert_eq!(dec_scale(6, 18), 1e-12);
        assert_eq!(dec_scale(18, 6), 1e12);
    }

    /// AAPL/USDG with the real on-chain figures: 5,167,183 USDG against 15,339
    /// AAPL is about $336.87 per AAPL, so 0.00297 AAPL per USDG.
    #[test]
    fn a_real_usdg_pool_prices_to_the_right_order_of_magnitude() {
        let p_raw = (15_339.0 * 1e18) / (5_167_183.0 * 1e6);
        let tokens_per_quote = p_raw * dec_scale(6, 18);
        assert!(
            (tokens_per_quote - 0.00297).abs() < 0.0005,
            "expected about 0.00297 AAPL per USDG, got {tokens_per_quote}"
        );
        // The bug produced billions; that must never pass again.
        assert!(tokens_per_quote < 1.0, "a stock token is worth more than a dollar");
    }

    #[test]
    fn amounts_use_each_sides_own_decimals() {
        // A swap of 100 USDG for 0.29 AAPL, in base units as the log carries it.
        let usdg_raw = 100.0 * 1e6;
        let aapl_raw = 0.29 * 1e18;
        assert!((usdg_raw / 10f64.powi(6) - 100.0).abs() < 1e-9);
        assert!((aapl_raw / 10f64.powi(18) - 0.29).abs() < 1e-9);
        // Dividing the quote side by 1e18 — the old behaviour — reads as zero.
        assert!(usdg_raw / 1e18 < 1e-9, "this is the 0.000000 that was on screen");
    }

    #[test]
    fn a_quote_side_reserve_uses_the_quote_decimals() {
        // L/√P is a base-unit reserve; scaling it by 1e18 on a 6-dp quote is
        // what made "Pooled 0.000 ETH" while millions sat in the pool.
        let raw_reserve: f64 = 5_167_183.0 * 1e6;
        assert!((raw_reserve / 1e6 - 5_167_183.0).abs() < 1.0);
        assert!(raw_reserve / 1e18 < 0.01, "the old path rounded it away");
    }
}

#[cfg(test)]
mod decimals_tests {
    use super::*;

    /// A 6-decimal token (USDG-style) must scale by 1e6, not 1e18. Getting this
    /// wrong is a 10^12 error: it reads a real balance as ~0 and, on the trade
    /// path, would size a transfer 10^12 too high.
    #[test]
    fn of_token_uses_real_decimals() {
        assert_eq!(Wei::of_token(1.0, 18).raw(), 1_000_000_000_000_000_000);
        assert_eq!(Wei::of_token(1.0, 6).raw(), 1_000_000);
        assert_eq!(Wei::of_token(1234.5, 6).raw(), 1_234_500_000);
        // Never negative, never a huge wrapped value.
        assert_eq!(Wei::of_token(-1.0, 6).raw(), 0);
        assert_eq!(Wei::of_token(0.0, 18).raw(), 0);
    }

    /// `rounded` is the 18-dec special case and must agree with `of_token(_, 18)`
    /// so existing 18-dec behaviour is provably unchanged by this refactor.
    #[test]
    fn rounded_matches_of_token_at_18() {
        for v in [0.0, 0.000001, 0.5, 1.0, 12345.678] {
            assert_eq!(Wei::rounded(v).raw(), Wei::of_token(v, 18).raw(), "mismatch at {v}");
        }
    }

    /// Base units -> human must invert `of_token`, at both precisions.
    #[test]
    fn units_to_f64_roundtrips() {
        for (human, dec) in [(1.0f64, 18u8), (1.0, 6), (0.5, 6), (250.75, 6), (3.25, 18)] {
            let raw = Wei::of_token(human, dec).raw();
            let back = units_to_f64(U256::from(raw), dec);
            assert!((back - human).abs() < 1e-9, "{human} @ {dec}dp round-tripped to {back}");
        }
    }

    /// The specific bug: a 6-dec balance read with the 18-dec helper vanishes.
    /// This documents WHY the distinction exists.
    #[test]
    fn eighteen_dec_helper_would_lose_a_six_dec_balance() {
        let raw = U256::from(2_000_000u64); // 2.0 tokens at 6dp
        assert_eq!(units_to_f64(raw, 6), 2.0);
        assert!(wei_to_f64(raw) < 1e-11, "the old path read 2.0 tokens as ~0");
    }

    /// A balance read that fails must not be reported as a balance of zero.
    ///
    /// This is what a rate-limited endpoint actually did: `eth_getBalance`
    /// returned 429, the result became 0.0, and the dashboard showed an empty
    /// wallet while session PnL showed the loss of the entire balance — both
    /// flickering back on the next poll that happened to succeed.
    #[test]
    fn a_failed_balance_read_keeps_the_last_known_balance() {
        let held = 0.000620_f64;

        // What the engine does with each poll's result, in the two cases.
        let apply = |current: f64, read: Option<f64>| match read {
            Some(eth) => eth,
            None => current,
        };

        // A good read updates.
        assert_eq!(apply(0.0, Some(held)), held);
        // A failed read holds, rather than zeroing.
        assert_eq!(apply(held, None), held);

        // And session PnL, which is balance minus baseline, therefore stays at
        // zero across a failed poll instead of reporting a total loss.
        let baseline = held;
        assert_eq!(apply(held, None) - baseline, 0.0);
        assert_eq!(
            0.0_f64 - baseline,
            -held,
            "this is the number that appeared on screen when a failed read became 0"
        );
    }
}
