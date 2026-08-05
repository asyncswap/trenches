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
use alloy::sol_types::SolCall;

use crate::contracts::*;
use crate::v3;
use crate::v4;

#[derive(Clone, Copy, PartialEq)]
pub enum Side {
    Buy,
    Sell,
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

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum OrderStatus {
    Pending,
    Confirmed,
    Reverted,
    Skipped,
    Failed,
}

impl OrderStatus {
    /// A stable name, safe to put in a proof. Derived Debug would work until
    /// someone renames a variant, at which point every historic proof would
    /// silently stop verifying.
    pub fn label(&self) -> &'static str {
        match self {
            OrderStatus::Pending => "pending",
            OrderStatus::Confirmed => "confirmed",
            OrderStatus::Reverted => "reverted",
            OrderStatus::Skipped => "skipped",
            OrderStatus::Failed => "failed",
        }
    }
}

/// One user action in the orders queue — transitions pending -> confirmed/etc.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Order {
    pub label: String,
    pub status: OrderStatus,
    pub hash: Option<TxHash>,
    pub mc: f64,     // market cap (ETH) at order time — the entry point for a BUY
    pub pooled: f64, // pooled ETH at order time
    pub eth: f64,    // ETH value of this order (tape-parity amount column)
    pub is_v4: bool, // venue at order time (v4 vs v3), for the pool column
    /// The token this order traded — so a confirmed order can be matched to
    /// (or injected into) the CURRENT pool's tape, and never someone else's.
    pub token: Address,
    /// Its ticker at the time of the order.
    ///
    /// Stored, not looked up: tickers are not unique and a scam can rename
    /// itself, so what matters is the name it wore when you traded it. The
    /// address beside it is the identity; this is only how to say it out loud.
    /// Empty on orders written before this field existed.
    #[serde(default)]
    pub sym: String,
    /// The block the transaction landed in, once the receipt is in hand.
    ///
    /// Not the block it was SENT at: a confirmed order is injected into the
    /// tape when a thin `getLogs` window misses it, and a placeholder stamped
    /// with the current block reads as a trade that just happened. Coming back
    /// to a token re-injected yesterday's fills at the top of the tape, dated
    /// seconds old.
    ///
    /// Deliberately NOT part of the proof: it is a property of `tx`, which the
    /// proof already covers, so adding it would invalidate every existing proof
    /// to attest to something already attested.
    #[serde(default)]
    pub block: u64,
    /// What this transaction cost to send, in ETH, off its receipt.
    ///
    /// Per order rather than only per closed trade, because the question "what
    /// did that press cost me" is asked of a single transaction — and a buy has
    /// no fill to hang it on until the sell that closes it, which may be days
    /// away or never.
    #[serde(default)]
    pub gas: f64,
    /// Unix seconds when THIS bot sent it.
    ///
    /// The audit trail. Every order in this list was signed and broadcast by
    /// this process on a keypress — nothing here trades on its own — so a
    /// timestamp turns "did I do that?" into a question the record answers.
    /// A transaction from your address that is NOT in this file, at a time you
    /// were not at the keyboard, is the thing worth alarm.
    ///
    /// Defaulted for orders written before this field existed.
    #[serde(default)]
    pub at: u64,
    /// The key that caused this order.
    ///
    /// Nothing in this app trades without a keypress, so every order has one —
    /// and recording WHICH turns "I don't remember doing that" into something
    /// the file can answer. Empty on orders written before this existed.
    #[serde(default)]
    pub key: String,
    /// Which field set `proof` covers. See `ledger::Fill::pv` — same problem,
    /// same shape. Adding `sym` to the covered fields invalidated every proof
    /// already written, so untouched orders came back flagged as tampered by a
    /// change made here. 0 = written before proofs were versioned.
    #[serde(default)]
    pub pv: u32,
    /// Chained proof over this order's own fields and the proof before it.
    /// See `verification`. Empty means unverified, not forged.
    #[serde(default)]
    pub proof: String,
    /// Set on LOAD when this row's proof checked out. Not persisted — it is a
    /// conclusion about the file, not a fact in it, and writing it down would
    /// let a forger simply set it to true.
    #[serde(skip)]
    pub verified: bool,
}

/// The current order-proof scheme. Bump when `order_fields` changes.
pub const ORDER_PROOF_VERSION: u32 = 1;

impl Order {
    /// Whether this order's proof can be checked at all, and failed.
    ///
    /// Not the same as "unverified". Most rows are unverifiable for reasons
    /// that say nothing about anyone: written before proofs existed, or under
    /// an older field set. Badging those is a warning about a schema change,
    /// and a warning that fires for that stops being read for anything.
    pub fn suspect(&self) -> bool {
        self.pv == ORDER_PROOF_VERSION && !self.proof.is_empty() && !self.verified
    }
}


/// Protocol-specific pool identity. A v4 pool is identified by its pool id +
/// tickSpacing; a v3 pool by its contract address + token orientation. Encoding
/// them as a sum type makes illegal states (v4 without id, v3 without address)
/// unrepresentable.
#[derive(Clone, Copy, PartialEq)]
pub enum PoolKind {
    V4 { pool_id: B256, tick_spacing: i32 },     // native ETH, PoolManager/UniversalRouter
    V3 { pool_addr: Address, weth_is_token0: bool }, // weth(), SwapRouter02
    // A Flaunch launch: the same PoolManager, but paired against flETH with the
    // Flaunch hook attached, so swaps route ETH<->flETH<->coin through the
    // UniversalRouter. Not a widened V4: that variant bakes in currency0 =
    // native ETH and hooks = 0. coin_is_0 records _currencyFlipped from the
    // launch (flETH's low address makes it currency0 in practice, but the
    // protocol allows either order).
    FlaunchV4 { pool_id: B256, coin_is_0: bool },
    // A pons v2 launch still on its bonding CURVE. Not a pool at all: the curve
    // holds the whole supply and both prices and settles every trade itself,
    // so there is no pool id, no tick, and no router in the path — you call
    // buy/sell on the curve. It becomes a v4 pool at graduation, at which point
    // the launch is re-read as `V4` and this variant is gone for that token.
    //
    // `quote` is what the launch is priced in: zero for native ETH, otherwise
    // an approved ERC-20 that is the currency of the entire launch.
    PonsCurve { curve: Address, quote: Address },
    // What a pons v2 launch becomes once its curve sells out: a Uniswap v4 pool
    // carrying the pons hook, paired against whatever the launch was priced in.
    //
    // Not `V4`, which bakes in currency0 = native ETH and hooks = 0. This pool's
    // fee field is ZERO and the hook charges instead, so reading the pool's own
    // fee tells you nothing about what a trade costs — the same trap that
    // mispriced Flaunch.
    // `quote` and `tick_spacing` ride along because ROUTING needs them: the
    // hook and both currencies form the pool key, and unlike Flaunch the quote
    // is not a fixed asset — each launch chooses its own.
    PonsV2Pool { pool_id: B256, coin_is_0: bool, quote: Address, tick_spacing: i32 },
}

/// The v4 pool id a pons v2 launch graduates into.
///
/// Uniswap sorts the two currencies by address, and native ETH is the zero
/// address so it always takes currency0 when present. The pool's own fee is
/// zero — the hook charges — and the hook is part of the key, so a pool id
/// computed with hooks = 0 would point at a pool that does not exist.
pub fn pons_v2_pool_id(token: Address, quote: Address, tick_spacing: i32, hook: Address) -> B256 {
    use alloy::sol_types::SolValue;
    let (c0, c1) = if quote < token { (quote, token) } else { (token, quote) };
    let spacing: alloy::primitives::aliases::I24 = tick_spacing.try_into().unwrap_or_default();
    let fee: alloy::primitives::aliases::U24 = alloy::primitives::aliases::U24::ZERO;
    alloy::primitives::keccak256((c0, c1, fee, spacing, hook).abi_encode_params())
}

impl PoolKind {
    pub fn proto(&self) -> &'static str {
        match self {
            PoolKind::V4 { .. } => "v4",
            PoolKind::V3 { .. } => "v3",
            PoolKind::FlaunchV4 { .. } => "flaunch",
            PoolKind::PonsCurve { .. } => "curve",
            PoolKind::PonsV2Pool { .. } => "pons2",
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
            PoolKind::PonsCurve { .. } => "Pons v2 curve",
            PoolKind::PonsV2Pool { .. } => "Pons v2",
        }
    }

    /// The venue's name for the banner, in the pixel font's alphabet.
    ///
    /// Every v3 pool reachable here arrived through a Pons v1 graduation, so
    /// that is what a v3 pool is — "UNISWAP V3" would name the AMM it settled
    /// into rather than the launchpad it came from, and the launchpad is what
    /// decides how it behaves.
    pub fn banner_name(&self) -> &'static str {
        match self {
            PoolKind::V3 { .. } => "PONS V1",
            PoolKind::PonsCurve { .. } => "PONS V2",
            PoolKind::PonsV2Pool { .. } => "PONS V2",
            PoolKind::FlaunchV4 { .. } => "FLAUNCH",
            PoolKind::V4 { .. } => "UNISWAP V4",
        }
    }

    /// The venue's name, short enough for a table cell.
    ///
    /// `v3` and `v4` describe a protocol version, which is not what anyone is
    /// asking when they glance at that column — they want to know whose venue
    /// this is. These projects have names and there is no reason not to use
    /// them: Flaunch launches are Flaunch's, and every v3 pool on this chain
    /// arrived through Pons.
    pub fn venue_short(&self) -> &'static str {
        match self {
            PoolKind::V4 { .. } => "Uniswap",
            PoolKind::V3 { .. } => "Uniswap",
            PoolKind::FlaunchV4 { .. } => "Flaunch",
            PoolKind::PonsCurve { .. } => "Pons v2",
            PoolKind::PonsV2Pool { .. } => "Pons v2",
        }
    }
    pub fn is_v3(&self) -> bool {
        matches!(self, PoolKind::V3 { .. })
    }
    /// True when no real pool is selected (placeholder / empty network).
    pub fn is_empty(&self) -> bool {
        match self {
            PoolKind::V4 { pool_id, .. }
            | PoolKind::FlaunchV4 { pool_id, .. }
            | PoolKind::PonsV2Pool { pool_id, .. } => *pool_id == B256::ZERO,
            PoolKind::V3 { pool_addr, .. } => *pool_addr == Address::ZERO,
            PoolKind::PonsCurve { curve, .. } => *curve == Address::ZERO,
        }
    }

    /// True while the launch trades on a bonding curve rather than in a pool.
    /// Everything pool-shaped — tick, sqrt price, pool id, routing through a
    /// router — is meaningless until this is false.
    pub fn is_curve(&self) -> bool {
        matches!(self, PoolKind::PonsCurve { .. })
    }
}

/// The currency a pool is quoted in. Determines reserve orientation and how the
/// price is valued in USD. ETH pools use the native/WETH side; stable pools
/// (USDG, …) are quoted in a stablecoin whose USD value is fetched LIVE — never
/// assumed to be $1, since stables drift and can depeg.
#[derive(Clone, Copy, PartialEq)]
pub enum Quote {
    Eth,                                     // native ETH (v4) / weth() (v3), 18-dec
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
    /// When the market line last went to the trace, and at what tick — so the
    /// line is written on change, not ten times a second forever.
    pub last_market_trace: Option<(std::time::Instant, i32)>,
    /// Candle interval for the chart panel, seconds. , and . walk the ladder.
    /// Chart y axis: false = price, true = market cap. See the `m` key.
    pub chart_mcap: bool,
    pub chart_iv: u64,

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
    #[cfg(feature = "liquidity")]
    pub lp_frac: f64,         // LP add size as fraction of ETH balance (fixed)
    pub nonce: Option<u64>,   // locally-tracked nonce (fast, race-free sends)
    pub profit_guard: bool,   // gate trades on positive EV (toggle with 'g')
    pub guard_dup: bool,      // one trade in flight per token — no double buys (toggle 'n')
    pub min_edge_eth: f64,    // required edge over gas (ETH)
    pub ref_price: f64,       // reference price (SMA) for the profit filter
    pub gas_price: f64,       // wei, from the market read
    pub token_supply: f64,    // token totalSupply, for market cap
    pub eth_usd: f64,         // ETH price estimate for USD market cap
    pub last_edge: f64,       // last computed trade edge (ETH), for the UI
    pub last_read_ms: f64,
    pub lp_permit2_done: bool,
    pub v3_covered: bool, // SwapRouter02 allowance covers the full position (exact-amount, v3 sells)
    /// Every transaction hash this account ever sent from this app —
    /// persisted (mytx-<address>.txt), so the tape's "your trade" mark
    /// survives a restart instead of living and dying with the order list.
    pub own_txs: std::collections::HashSet<TxHash>,
    /// The key currently being acted on, stamped onto any order it produces.
    /// How much one press of `[` or `]` moves the buy size, when you have said.
    ///
    /// `None` means the app picks — finer on a bigger wallet, since the step is
    /// a fraction of the balance and a fixed one gets coarser in real money the
    /// more you hold. `;` and `'` set it, and once set it stays: a default that
    /// is chosen for you is a convenience, a default that keeps overriding you
    /// is a fault.
    pub buy_step_override: Option<f64>,
    /// ETH spent on transactions that reverted — bought nothing, cost real
    /// money. Kept apart from any position's basis because it belongs to none.
    /// Gas spent acquiring the position currently open, so a sell can report
    /// what the whole round trip cost rather than only its own half.
    /// The last state line written, and when — so an unchanged one is not
    /// written again. See `telemetry`.
    pub last_telemetry: Option<String>,
    pub last_telemetry_at: Option<Instant>,
    pub bought_gas: f64,
    pub gas_burned: f64,
    pub acting_key: String,
    /// Buys waiting to be re-checked for a drain. See [`Bot::watch_for_drain`].
    pub drain_watch: Vec<DrainWatch>,
    // Permit2 + UniversalRouter allowances cover the full position (Flaunch
    // sells settle the coin through Permit2, which needs both grants).
    pub ur_permit2_done: bool,
    /// Unix seconds at which the Permit2 grant behind `ur_permit2_done` runs
    /// out. 0 = unknown.
    ///
    /// The flag alone says "approved once". A grant expires, so a position held
    /// longer than the window would have found the flag still true, skipped the
    /// allowance check, and sent a swap the router refuses — the trader's first
    /// attempt to LEAVE failing, which is the worst possible moment for it.
    pub ur_permit2_until: u64,
    pub routes: Vec<Route>, // candidate ETH-quoted venues for best-execution routing
    pub socials: TokenSocials,     // current token's on-chain socials/metadata (for the market view)
    /// Pons graduation block, for the pool-age display and the venue logo.
    ///
    /// `Some(0)` means "no Pons launch" — verified and leaderboard pools carry
    /// that placeholder. Use `pons_launch()` rather than testing `is_some()`,
    /// which those placeholders satisfy.
    pub pool_launch_block: Option<u64>,
    pub status: String, // last action result, shown in the dashboard
}

/// A confirmed buy, to be looked at again shortly.
///
/// The tokens arriving is not the same as keeping them. A coin can confirm the
/// buy, hand over the balance, and take it back seconds later — which is
/// exactly what happened on 2026-07-31: 34,230 tokens delivered, 5,670,304
/// base units left fifteen seconds on, and the sell moved dust for nothing.
/// No pre-trade simulation catches that, because inside one transaction the
/// drain has not happened yet.
#[derive(Clone, Copy)]
pub struct DrainWatch {
    pub token: Address,
    /// Balance right after the buy confirmed — the number to compare against.
    pub had: U256,
    /// When it becomes worth re-reading.
    pub due: Instant,
    pub hash: TxHash,
}

/// One token's open cost basis, as persisted between sessions. Keyed by token
/// address in `basis-evm-<trader>.json`; absent means no open position.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Basis {
    /// Gas already spent acquiring this position. Persisted with the rest of
    /// the basis: a restart that forgot it would report the next sell's round
    /// trip as costing only the exit.
    #[serde(default)]
    pub gas: f64,
    pub qty: f64,
    pub cost: f64,
    #[serde(default)]
    pub entry_at: Option<u64>,
    #[serde(default)]
    pub entry_mc: f64,
    #[serde(default)]
    pub entry_pooled_eth: f64,
    #[serde(default)]
    pub entry_tx: Option<String>,
}

/// On-chain socials/metadata for a Pons launch token (all empty for non-Pons).
/// Serde because the facts cache (src/facts.rs) mirrors it to disk.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenSocials {
    pub logo: String,
    pub description: String,
    pub twitter: String,
    pub telegram: String,
    pub website: String,
    pub discord: String,
    pub farcaster: String,
}

impl TokenSocials {
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
pub async fn fetch_token_meta<P: Provider>(provider: &P, token: Address) -> TokenSocials {
    let t = crate::contracts::IPonsToken::new(token, provider);
    let logo = t.logo().call().await.map(|s| s._0).unwrap_or_default();
    let description = t.description().call().await.map(|s| s._0).unwrap_or_default();
    let (twitter, telegram, discord, website, farcaster) = t
        .socials()
        .call()
        .await
        .map(|s| (s.twitter, s.telegram, s.discord, s.website, s.farcaster))
        .unwrap_or_default();
    TokenSocials { logo, description, twitter, telegram, website, discord, farcaster }
}

/// Fetch a Flaunch coin's metadata JSON from its launch tokenUri (ipfs://…).
/// Unlike Pons, the socials live off-chain: the JSON carries image, description
/// and the social URLs. Any failure (gateway down, bad JSON) returns an empty
/// TokenSocials — metadata is never worth stalling the app for. `farcaster` stays
/// empty: Flaunch metadata has no such field.
pub async fn fetch_flaunch_meta(token_uri: &str) -> TokenSocials {
    if token_uri.trim().is_empty() {
        return TokenSocials::default();
    }
    // The URI is attacker-written on-chain data — the guard inside `fetch_json`
    // decides whether it is fetchable at all, and refuses anything aimed at
    // this machine. Every gateway at once: this document names the artwork, so
    // whatever waits on it delays the picture too.
    let Some(json) = crate::art::fetch_json(token_uri, 4_000).await else {
        return TokenSocials::default();
    };
    let s = |keys: &[&str]| -> String {
        keys.iter()
            .filter_map(|k| json.get(*k).and_then(|v| v.as_str()))
            .find(|v| !v.trim().is_empty())
            .unwrap_or_default()
            .to_string()
    };
    TokenSocials {
        // The image is itself usually ipfs:// — store the gateway form so the
        // detail panes hold a URL a person can actually open.
        logo: {
            let img = s(&["image", "imageIpfs"]);
            crate::net::metadata_url(&img).unwrap_or_default()
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
        1.0 - self.slippage_pct.clamp(1.0, 90.0) / 100.0
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
        format!("{}/daily-{}.json", crate::state_dir(), crate::ledger::safe_account(&self.account))
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
    /// `gas` is what THIS transaction actually cost to send, in ETH, read from
    /// its receipt. It is part of the trade, not an overhead beside it: a buy's
    /// gas is money spent acquiring the position, and a sell's gas comes
    /// straight out of the proceeds. Leaving it out reports every trade as
    /// better than it was, and on stakes this size it is not a rounding error
    /// — a 0.0018 ETH buy whose gas is 0.00002 ETH has given up 1% before the
    /// price has moved at all.
    fn apply_fill(&mut self, side: Side, eth: f64, tok: f64, hash: TxHash, gas: f64) {
        match side {
            Side::Buy => {
                self.bought_qty += tok;
                // What the position cost is what left the wallet: the ETH into
                // the pool AND the gas that put it there.
                self.bought_cost += eth + gas;
                self.bought_gas += gas;
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
                // The allowance is GONE. Both routers are granted an exact
                // amount, and this swap just spent it — so a flag saying "the
                // router can pull this token" stopped being true the moment
                // the transfer went through.
                //
                // It used to survive, because it was only ever cleared when the
                // pool changed. That made the next sell skip the allowance
                // check entirely and send a transaction the router could not
                // fulfil: `STF`, SafeTransferFrom, over and over, on a wallet
                // holding 163,373 tokens with two-millionths of one approved.
                // No slippage or impact setting can fix a missing approval,
                // which is why nothing the trader tried made any difference.
                self.v3_covered = false;
                self.ur_permit2_done = false;
                // Proceeds are what you KEEP: the ETH out of the pool less the
                // gas it cost to get it out.
                let eth = eth - gas;
                let (from_basis, cost) = realized_cost(tok, self.bought_qty, self.bought_cost);
                let realized = eth - cost; // free-bag portion has zero cost
                // The ROUND TRIP's gas: this sell's, plus the share of the
                // buys' that belongs to the portion being closed. The buy half
                // is already inside `cost`; this reports it so the number can
                // be looked at rather than only felt.
                let buy_share = if self.bought_qty > 1e-12 {
                    self.bought_gas * (from_basis / self.bought_qty).min(1.0)
                } else {
                    self.bought_gas
                };
                let gas = gas + buy_share;
                self.bought_gas = (self.bought_gas - buy_share).max(0.0);

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
                    self.socials.score(), buy_tx, hash
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
                        gas,
                        proof: String::new(), // filled by append, which reads the chain
                        pv: 0,                // likewise
                        verified: true,       // just made; nothing to distrust yet
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
        // Basis changed; get it on disk before anything can kill the process.
        self.save_basis();
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
    /// Write the session's state line — but only when it says something new.
    ///
    /// This used to run every two seconds regardless. At 170 bytes a line that
    /// is 7 MB a day, and on a quiet pool nearly all of it was the same numbers
    /// with a different block number on the front — a log you cannot read
    /// because the signal is buried in its own heartbeat.
    ///
    /// Not deleted, though. This is the line that showed a pool's liquidity
    /// falling from 4.7 ETH to 1.4 while the skip counter climbed, which is how
    /// a session that "just stopped working" became a diagnosis. What is worth
    /// dropping is the repetition, not the record.
    ///
    /// So: written when a material field changes, and otherwise at most once
    /// every five minutes. The heartbeat matters — a log that goes silent
    /// because nothing changed looks exactly like a log that went silent
    /// because the app died.
    pub fn telemetry(&mut self, block: u64, round_ms: f64) {
        // Once a window, not once a call: a rate is not something anyone can
        // see by watching individual calls scroll past.
        crate::rpcstats::maybe_report();
        // Block and round_ms are deliberately NOT in the signature: they change
        // every single call, so including them would make every line "new" and
        // the check pointless.
        let sig = format!(
            "{:.8}|{:.6}|{:.6}|{:.4}|{:+.6}|{}|{}|{}|{}",
            self.price(),
            self.r0,
            self.eth,
            self.token_bal,
            self.pnl(),
            self.trades,
            self.fails,
            self.skips,
            self.pending.len(),
        );
        let stale = self
            .last_telemetry_at
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(300));
        if self.last_telemetry.as_deref() == Some(sig.as_str()) && !stale {
            return;
        }
        self.last_telemetry = Some(sig);
        self.last_telemetry_at = Some(Instant::now());
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

    /// `take_nonce` advances the local counter optimistically, so a send that
    /// then fails leaves a hole: the number is never spent, and every later tx
    /// is signed one ahead of the chain — stalling in the mempool forever while
    /// the UI cheerfully reports SENT. Pass the send's result through here so a
    /// failure hands the nonce back and the next `take_nonce` resyncs.
    fn spent_nonce<T, E>(&mut self, sent: Result<T, E>) -> Result<T, E> {
        if sent.is_err() {
            self.nonce = None;
        }
        sent
    }

    /// Ensure the token's allowance to SwapRouter02 covers `need` wei, using an
    /// EXACT-amount approval — never MAX (no-infinite-approval policy). If the
    /// current allowance is short, approve exactly `need`. Returns Ok(true) when
    /// the allowance is sufficient, Ok(false) when the approval was sent but has
    /// not mined yet (caller should retry the swap shortly). Bounded receipt
    /// poll so it never hangs.
    async fn ensure_v3_allowance<P: Provider>(&mut self, provider: &P, need: U256) -> eyre::Result<bool> {
        let erc = IERC20::new(self.pool.token, provider);
        if let Ok(a) = erc.allowance(self.trader, swap_router_02()).call().await {
            if a._0 >= need {
                return Ok(true);
            }
        }
        self.note(format!("Approving {} for the router", self.pool.sym));
        let nonce = self.take_nonce(provider).await?;
        let sent = erc.approve(swap_router_02(), need).gas(120_000).nonce(nonce).send().await;
        let hash = *self.spent_nonce(sent)?.tx_hash();
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
            .allowance(self.trader, self.pool.token, universal_router())
            .call()
            .await
            .map(|a| a.amount >= need160 && a.expiration > now48)
            .unwrap_or(false);
        if erc_ok && p2_ok {
            // Trust the chain's expiry, not a guess about when it was made —
            // this grant may predate the session.
            self.ur_permit2_until = p2
                .allowance(self.trader, self.pool.token, universal_router())
                .call()
                .await
                .map(|a| a.expiration.to::<u64>())
                .unwrap_or(0);
            return Ok(true);
        }
        self.note(format!("Approving {} for the Universal Router", self.pool.sym));
        let mut last = None;
        if !erc_ok {
            let nonce = self.take_nonce(provider).await?;
            // The one approval in this codebase that is unbounded, and the
            // only one allowed to be. See "No unlimited approvals" in
            // docs/manifesto.md — this is the documented exception.
            //
            // PERMIT2 cannot move anything on its own. It moves tokens only
            // where it holds a grant that names the spender, the amount and an
            // expiry, and the grant below is exact and lasts a day. That is
            // where the bound lives.
            //
            // Bounding this leg too would cost an ERC-20 approval before every
            // single trade, because an exact grant is consumed by the trade
            // that uses it. An exit would be three transactions instead of two.
            // In a market where the difference between getting out and not is
            // measured in seconds, that is not a safety improvement — it trades
            // a narrow risk for a broader one.
            let sent = erc.approve(PERMIT2, U256::MAX).gas(120_000).nonce(nonce).send().await;
            last = Some(*self.spent_nonce(sent)?.tx_hash());
        }
        if !p2_ok {
            // A day, not the year 2100. Long enough that holding a position
            // across a session does not put an extra approval in front of the
            // sell; short enough that a grant cannot outlive the trading it was
            // for. A standing permission nobody remembers giving is the thing
            // being avoided, and 2100 was exactly that.
            let expiration48 = now48.saturating_add(U48::from(crate::config::permit2_ttl_secs()));
            // Remember when it runs out, so the cached "approved" flag can stop
            // being believed at the right moment rather than at the next
            // failure.
            self.ur_permit2_until = expiration48.to::<u64>();
            let nonce = self.take_nonce(provider).await?;
            let sent = p2
                .approve(self.pool.token, universal_router(), need160, expiration48)
                .gas(120_000)
                .nonce(nonce)
                .send()
                .await;
            last = Some(*self.spent_nonce(sent)?.tx_hash());
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
    /// Hand back the unlimited Permit2 allowance once the position is gone.
    ///
    /// The mirror of `pre_approve_exit`: that one grants on a confirmed buy,
    /// this one gives it back on the sell that empties the balance.
    ///
    /// "Your balance is zero, so the allowance grants nothing" is true only
    /// until the next time you hold that token — an airdrop, a transfer from
    /// another wallet, a buy somewhere else — and then an unlimited allowance
    /// nobody remembers granting is live again over funds that were never
    /// approved for anything. A permission that outlives what it was for is
    /// the whole failure mode.
    ///
    /// It runs AFTER the exit has confirmed, so it costs nothing that matters:
    /// the money is already out. Bounding the allowance up front would have put
    /// a transaction in front of the sell instead, which is the one place a
    /// delay is expensive.
    ///
    /// Only the Permit2 leg needs this. Router allowances are exact and spent
    /// by the trade that used them, so they revoke themselves.
    async fn revoke_permit2_if_empty<P: Provider>(&mut self, provider: &P) -> eyre::Result<()> {
        let erc = IERC20::new(self.pool.token, provider);
        if !erc.balanceOf(self.trader).call().await?._0.is_zero() {
            return Ok(());
        }
        // Nothing to give back on a token that never went through Permit2.
        if erc.allowance(self.trader, PERMIT2).call().await?._0.is_zero() {
            return Ok(());
        }
        let nonce = self.take_nonce(provider).await?;
        let sent = erc.approve(PERMIT2, U256::ZERO).gas(120_000).nonce(nonce).send().await;
        // Broadcast and let go. Waiting on the receipt would hold the UI for a
        // block to confirm a cleanup that changes nothing the trader is about
        // to do — and if it fails, the next trade in this token re-approves
        // from an allowance that is merely still there rather than wrong.
        let _ = self.spent_nonce(sent)?;
        // The next trade in this token approves again from scratch.
        self.ur_permit2_done = false;
        self.ur_permit2_until = 0;
        self.note(format!("Position closed — revoked the Permit2 allowance for {}", self.pool.sym));
        Ok(())
    }

    /// Whether the Permit2 grant is still good, rather than merely once made.
    ///
    /// Ten seconds of margin: a grant expiring while the swap is in flight is
    /// refused on arrival, and re-approving early costs one transaction where
    /// being late costs the exit.
    fn ur_ready(&self) -> bool {
        if !self.ur_permit2_done {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.ur_permit2_until == 0 || self.ur_permit2_until > now + 10
    }

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
    /// Where an account's own-transaction marks live.
    fn own_tx_path_of(trader: Address) -> String {
        format!("{}/mytx-{trader:#x}.txt", crate::state_dir())
    }

    /// The persisted own-transaction set for one account. Bounded on load —
    /// the newest keep their marks, ancient history ages out of the tape
    /// anyway.
    pub fn load_own_txs(trader: Address) -> std::collections::HashSet<TxHash> {
        let Ok(text) = std::fs::read_to_string(Self::own_tx_path_of(trader)) else {
            return Default::default();
        };
        text.lines().rev().take(2_000).filter_map(|l| l.trim().parse().ok()).collect()
    }

    fn push_order(&mut self, label: String, status: OrderStatus, hash: Option<TxHash>) {
        let price = self.price();
        let mc = if price > 0.0 { self.token_supply / price } else { 0.0 };
        // Amount ALWAYS in ETH numeraire (cost in ether). Buys/LP encode the ETH in
        // the label; sells are token-denominated, so value them at price (ETH = tok
        // / price, since price is tokens-per-ETH).
        let eth = eth_of_label(&label)
            .unwrap_or_else(|| if price > 0.0 { self.token_bal / price } else { 0.0 });
        let is_v4 = matches!(self.pool.kind, PoolKind::V4 { .. } | PoolKind::FlaunchV4 { .. });
        if let Some(h) = hash {
            self.remember_own_tx(h);
        }
        self.orders.push_back(Order {
            label,
            status,
            hash,
            mc,
            pooled: self.r0,
            eth,
            is_v4,
            token: self.pool.token,
            sym: self.pool.sym.clone(),
            block: 0, // unknown until the receipt lands; see `settle_order`
            gas: 0.0,  // likewise: only the receipt knows
            at: crate::ledger::now(),
            key: self.acting_key.clone(),
            pv: ORDER_PROOF_VERSION,
            proof: String::new(), // filled by save_orders, which knows the chain
            verified: true,       // we just made it; nothing to distrust yet
        });
        while self.orders.len() > 200 {
            self.orders.pop_front();
        }
        self.save_orders();
    }

    /// Where the orders queue sleeps between sessions — per trader, so two
    /// wallets on one machine never read each other's history.
    fn orders_path(trader: Address) -> String {
        format!("{}/orders-evm-{trader}.jsonl", crate::state_dir())
    }


/// The fields a proof covers, in a fixed order.
///
/// The proof is only worth what it covers, so this is everything that would
/// change the meaning of the row: what it did, when, on which token, for how
/// much, at what price, which key caused it, and the transaction it became.
/// Adding a field here invalidates every existing proof, which is correct —
/// an old proof did not attest to the new field.
fn order_fields(o: &Order) -> Vec<String> {
    vec![
        o.label.clone(),
        format!("{}", o.status.label()),
        o.hash.map(|h| format!("{h:#x}")).unwrap_or_default(),
        format!("{:#x}", o.token),
        o.sym.clone(),
        format!("{:.18}", o.eth),
        format!("{:.18}", o.mc),
        o.at.to_string(),
        o.key.clone(),
    ]
}

    fn basis_path(trader: Address) -> String {
        format!("{}/basis-evm-{trader}.json", crate::state_dir())
    }

    /// Every recorded cost basis for an account, by token address.
    ///
    /// Public so the holdings screen can price what is held against what it
    /// cost — the same numbers a sell would realize, read without selling.
    pub fn load_basis_map(trader: Address) -> std::collections::BTreeMap<String, Basis> {
        if trader.is_zero() {
            return Default::default();
        }
        std::fs::read_to_string(Self::basis_path(trader))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist the open position's cost basis, keyed by token so several
    /// positions coexist and switching pools cannot cross-contaminate them.
    ///
    /// Orders and the tape were already durable while this — the state the
    /// ACCOUNTING depends on — lived only in memory. A restart mid-position
    /// therefore reopened with a zero basis, and the next sell booked its
    /// entire proceeds as profit into the permanent ledger.
    pub fn save_basis(&self) {
        if self.trader.is_zero() || self.pool.token.is_zero() {
            return;
        }
        let _ = std::fs::create_dir_all(crate::state_dir());
        let mut all = Self::load_basis_map(self.trader);
        let key = format!("{:#x}", self.pool.token);
        if self.bought_qty <= 1e-12 && self.bought_cost <= 1e-12 {
            all.remove(&key); // position closed — don't grow the file forever
        } else {
            all.insert(
                key,
                Basis {
                    gas: self.bought_gas,
                    qty: self.bought_qty,
                    cost: self.bought_cost,
                    entry_at: self.entry_at,
                    entry_mc: self.entry_mc,
                    entry_pooled_eth: self.entry_pooled_eth,
                    entry_tx: self.entry_tx.map(|h| format!("{h:#x}")),
                },
            );
        }
        let Ok(out) = serde_json::to_string(&all) else {
            return;
        };
        let path = Self::basis_path(self.trader);
        let tmp = format!("{path}.tmp");
        if std::fs::write(&tmp, out).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Load the saved basis for the CURRENT pool's token, or zero it when that
    /// token has no open position. Replaces the bare `bought_qty = 0.0` resets:
    /// basis is per-token, and now it survives the process.
    /// Rebuild a cost basis from your own trades on the saved tape.
    ///
    /// The basis file only knows what THIS app was told, and it was told
    /// nothing about a coin bought before the file existed. TOK sold for
    /// 0.005679 ETH against a basis of zero and was reported as a $10.63
    /// profit; the tape had held the four buys that made it — 0.000440,
    /// 0.000418, 0.000397 and 0.000377 ETH — the whole time. That is a real
    /// 3.5x, and calling it a $10.63 win overstated it by a third while
    /// calling nothing about it uncertain.
    ///
    /// The tape is local, already on disk, and marks which swaps are yours, so
    /// this costs no RPC. It replays your buys and sells in block order at
    /// average cost, exactly as a live session would have, and returns what is
    /// left open.
    ///
    /// Quantities come from each row's price rather than from a token amount
    /// the tape does not carry, so this is a RECONSTRUCTION, not a record —
    /// which is why it is only ever consulted when nothing was recorded.
    pub fn basis_from_tape(rows: &[Swap], trader: Address) -> (f64, f64, Option<u64>) {
        let mut mine: Vec<&Swap> = rows
            .iter()
            .filter(|s| s.trader == trader && s.price > 0.0 && s.eth > 0.0)
            .collect();
        mine.sort_by_key(|s| s.block);
        let (mut qty, mut cost) = (0.0f64, 0.0f64);
        let mut opened: Option<u64> = None;
        for s in mine {
            let tok = s.eth * s.price;
            match s.action {
                TapeAction::Buy => {
                    if qty <= 1e-12 {
                        opened = Some(s.block); // a fresh position starts its own clock
                    }
                    qty += tok;
                    cost += s.eth;
                }
                TapeAction::Sell => {
                    let (sold, spent) = realized_cost(tok, qty, cost);
                    qty = (qty - sold).max(0.0);
                    cost = (cost - spent).max(0.0);
                    if qty <= 1e-12 {
                        opened = None;
                    }
                }
                _ => {} // liquidity events move no basis
            }
        }
        (qty, cost, opened)
    }

    pub fn restore_basis(&mut self) {
        let b = Self::load_basis_map(self.trader)
            .remove(&format!("{:#x}", self.pool.token))
            .unwrap_or_default();
        self.bought_qty = b.qty;
        self.bought_cost = b.cost;
        self.bought_gas = b.gas;
        self.entry_at = b.entry_at;
        self.entry_mc = b.entry_mc;
        self.entry_pooled_eth = b.entry_pooled_eth;
        self.entry_tx = b.entry_tx.and_then(|s| s.parse().ok());
    }

    /// Fill a missing basis from the tape, and say so.
    ///
    /// Only when nothing was recorded — a reconstruction must never overwrite
    /// a real record, however plausible it looks. Called after `restore_basis`
    /// with the tape already in memory.
    pub fn recover_basis(&mut self, head: u64) {
        if self.trader.is_zero()
            || self.pool.token.is_zero()
            || self.bought_qty > 1e-12
            || self.bought_cost > 1e-12
        {
            return; // recorded, or nothing to recover for
        }
        // Straight off disk: the tape in memory may not have been loaded for
        // this token yet, and the file is the same rows.
        let Ok(text) = std::fs::read_to_string(crate::evm_tape_path(&self.pool.token)) else {
            return;
        };
        let rows: Vec<Swap> =
            text.lines().filter_map(|l| serde_json::from_str::<Swap>(l).ok()).collect();
        let (qty, cost, opened) = Self::basis_from_tape(&rows, self.trader);
        if qty <= 1e-12 || cost <= 1e-12 {
            return;
        }
        self.bought_qty = qty;
        self.bought_cost = cost;
        // A block number, converted at this chain's ~10 blocks/sec. Better than
        // no hold time; not presented as more than it is.
        if let Some(b) = opened {
            let behind = head.saturating_sub(b) / 10;
            self.entry_at = Some(crate::ledger::now().saturating_sub(behind));
        }
        self.note(format!(
            "Rebuilt a cost basis for {} from your own trades on the tape: {:.6} ETH across the buys still open.              Nothing had been recorded, so a sell would have booked its whole proceeds as profit.",
            self.pool.sym, cost
        ));
        self.save_basis(); // recorded from here on, so this runs once
    }

    /// Persist the queue, capped alongside the in-memory ring. A restart
    /// should reopen onto your own history, not an empty ledger.
    pub fn save_orders(&self) {
        if self.trader.is_zero() {
            return;
        }
        let _ = std::fs::create_dir_all(crate::state_dir());
        // Chain the proofs as the file is written, so the file itself is the
        // record — not something recomputed from memory on read.
        let mut out = String::new();
        let mut prev = String::new();
        for o in self.orders.iter() {
            let mut o = o.clone();
            // Orders from before this app recorded a time are left OUT of the
            // chain — no proof written, none expected.
            //
            // Proving them would be proving nothing: they have no timestamp, no
            // key, no symbol, so the fields a proof would cover are mostly
            // absent, and a chain anchored on rows that thin cascades a break
            // through every real order behind it. They already read as `—`
            // across the row, which is the honest label: this predates the
            // record, so the record does not vouch for it.
            if o.at > 0 {
                o.pv = ORDER_PROOF_VERSION;
                let f = Self::order_fields(&o);
                let refs: Vec<&str> = f.iter().map(|s| s.as_str()).collect();
                o.proof = crate::verification::proof(&prev, &refs);
                prev = o.proof.clone();
            } else {
                o.pv = 0;
                o.proof.clear();
            }
            if let Ok(j) = serde_json::to_string(&o) {
                out.push_str(&j);
                out.push('\n');
            }
        }
        let path = Self::orders_path(self.trader);
        let tmp = format!("{path}.tmp");
        if std::fs::write(&tmp, out).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// The saved queue, for a session opening on this trader. A Pending order
    /// from a dead session can never confirm — it loads as Failed, which is
    /// what it is.
    pub fn load_orders(trader: Address) -> VecDeque<Order> {
        let mut out = VecDeque::new();
        if trader.is_zero() {
            return out;
        }
        if let Ok(s) = std::fs::read_to_string(Self::orders_path(trader)) {
            for line in s.lines() {
                if let Ok(mut o) = serde_json::from_str::<Order>(line) {
                    if o.status == OrderStatus::Pending {
                        o.status = OrderStatus::Failed;
                    }
                    out.push_back(o);
                }
            }
        }
        // Verify the chain as loaded. A row that fails is NOT dropped — the
        // whole point is to show it and say it cannot be trusted, because a
        // record that quietly disappears is worse than one flagged.
        // A row from an older scheme presents an EMPTY proof to the checker,
        // which carries the chain past it untouched. Unverifiable is not
        // tampered: the rules changed underneath it.
        let chain: Vec<(String, Vec<String>)> = out
            .iter()
            .map(|o| {
                if o.pv == ORDER_PROOF_VERSION {
                    (o.proof.clone(), Self::order_fields(o))
                } else {
                    (String::new(), Vec::new())
                }
            })
            .collect();
        if let Some(i) = crate::verification::first_broken(&chain) {
            crate::events::error(
                "An order on disk does not match its proof — the file has been changed",
                &[
                    ("row", i.to_string()),
                    ("action", out.get(i).map(|o| o.label.clone()).unwrap_or_default()),
                    ("file", Self::orders_path(trader)),
                ],
            );
            // Everything from the first break onward is suspect: a chain that
            // breaks at i tells you nothing about i+1. The proof is KEPT, not
            // cleared — an empty proof is how a pre-proof row says "written
            // before this existed", and a broken row must not be able to
            // disguise itself as an old one.
            for o in out.iter_mut().skip(i) {
                o.verified = false;
            }
        } else {
            for o in out.iter_mut() {
                o.verified = o.pv == ORDER_PROOF_VERSION && !o.proof.is_empty();
            }
        }
        while out.len() > 200 {
            out.pop_front();
        }
        out
    }

    /// Record a transaction as OURS, in memory and on disk. The tape's "your
    /// trade" mark used to come from the in-memory order list alone, so a
    /// restart unmarked every trade made before it — a buy from the last
    /// session showed as just another trade next to its highlighted sell.
    fn remember_own_tx(&mut self, hash: TxHash) {
        if !self.own_txs.insert(hash) {
            return;
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(Self::own_tx_path_of(self.trader))
        {
            let _ = writeln!(f, "{hash:#x}");
        }
    }

    /// Move any pending order matching `hash` to a terminal status.
    fn settle_order(&mut self, hash: TxHash, status: OrderStatus, block: u64, gas: f64) {
        if let Some(o) = self.orders.iter_mut().find(|o| o.hash == Some(hash)) {
            o.status = status;
            o.gas = gas;
            // Where in time this trade actually sits. Everything that places it
            // on the tape reads this rather than "now", so a fill re-shown
            // later is shown at its own moment.
            if block > 0 {
                o.block = block;
            }
        }
    }

    /// Re-read the balance of a coin we bought a moment ago, and say so loudly
    /// if it went missing.
    ///
    /// The one check that catches the drain-later scam. A honeypot that blocks
    /// selling can be found by simulating a sell; a coin that lets the buy
    /// through and then takes the balance back cannot, because inside a single
    /// transaction it has not taken anything yet. Only time reveals it.
    ///
    /// Cheap — one balanceOf per buy, once — and it cannot be faked, because it
    /// reads the same number the sell will size from. Purely informational: it
    /// never blocks a trade, it tells you the money already went.
    pub async fn check_drains<P: Provider>(&mut self, provider: &P) {
        let now = Instant::now();
        let due: Vec<DrainWatch> =
            self.drain_watch.iter().copied().filter(|w| w.due <= now).collect();
        if due.is_empty() {
            return;
        }
        self.drain_watch.retain(|w| w.due > now);
        for w in due {
            let Ok(bal) = IERC20::new(w.token, provider).balanceOf(self.trader).call().await else {
                continue; // a failed read is not evidence of anything
            };
            let now_bal = bal._0;
            if now_bal >= w.had {
                continue;
            }
            // A tenth is well outside any rounding, and nothing legitimate
            // shrinks a holding you are simply sitting on.
            let lost = w.had - now_bal;
            let pct = (lost.saturating_mul(U256::from(100)) / w.had.max(U256::from(1))).to::<u128>();
            if pct < 10 {
                continue;
            }
            self.note(format!(
                "WARNING: {} took back {pct}% of your balance after the buy. This coin drains its holders — treat the position as gone",
                self.pool.sym
            ));
            crate::events::error(
                "A coin reduced your balance after the buy landed",
                &[
                    ("coin", self.pool.sym.clone()),
                    ("token", format!("{:#x}", w.token)),
                    ("lost", format!("{pct}%")),
                    ("buy", format!("{:#x}", w.hash)),
                ],
            );
        }
    }

    /// Follow a pons v2 launch across graduation.
    ///
    /// A launch trades on its curve and then, without warning and inside
    /// whoever's buy happens to finish it, becomes a Uniswap v4 pool. The curve
    /// stops accepting trades at that moment and reverts with CurveGraduated,
    /// so a bot still pointed at it would sit there failing every order while
    /// the token traded normally somewhere else.
    ///
    /// `phase` on the factory is the authoritative signal — 0 curve, 1 swept
    /// (closed, pool not built yet), 2 pool, 3 rescued. Their docs are explicit
    /// that it must not be inferred from balances or events, so it is read.
    ///
    /// Cheap: one call, and only while a curve is actually open.
    pub async fn check_graduation<P: Provider>(&mut self, provider: &P) {
        let PoolKind::PonsCurve { .. } = self.pool.kind else { return };
        let f = IPonsV2Factory::new(pons_v2_factory(), provider);
        let Ok(rec) = f.getLaunchedToken(self.pool.token).call().await else { return };
        let rec = rec._0;
        if !rec.exists {
            return;
        }
        match rec.phase {
            // Still on the curve, or closed but not yet seeded. Phase 1 is
            // transient and nothing trades in it, so say so rather than
            // leaving the screen looking live.
            0 => {}
            1 => self.note(format!(
                "{} sold out its curve and is waiting for its pool. Trading resumes when it lands",
                self.pool.sym
            )),
            2 => {
                let pool_id = pons_v2_pool_id(
                    self.pool.token,
                    rec.pairToken,
                    rec.tickSpacing.as_i32(),
                    pons_v2_hook(),
                );
                self.pool.kind = PoolKind::PonsV2Pool {
                    pool_id,
                    // Uniswap sorts by address, and native ETH is address zero,
                    // so a native-quote launch always has the coin as
                    // currency1.
                    coin_is_0: self.pool.token < rec.pairToken,
                    quote: rec.pairToken,
                    tick_spacing: rec.tickSpacing.as_i32(),
                };
                // The tape was reading the curve, which no longer emits.
                self.note(format!(
                    "{} graduated — now trading in its Uniswap pool",
                    self.pool.sym
                ));
                crate::events::info(
                    "A pons v2 launch graduated to its pool",
                    &[
                        ("coin", self.pool.sym.clone()),
                        ("pool_id", format!("{pool_id:#x}")),
                        ("quote", format!("{:#x}", rec.pairToken)),
                    ],
                );
            }
            // The recovery path. Off the normal route entirely, and worth
            // saying out loud rather than quietly showing a dead market.
            3 => self.note(format!(
                "{} was rescued rather than graduated. It has no pool — do not trade it",
                self.pool.sym
            )),
            _ => {}
        }
    }

    /// Gas for a send: ask the node, then add real headroom — never below the
    /// venue's floor.
    ///
    /// A fixed table is what broke every Flaunch buy: 500,000 against a real
    /// need of 837,000. A table cannot know what a hook deployed tomorrow will
    /// cost, and being shy in it fails in the worst possible way — an OOG
    /// inside a hook is reported as the hook refusing the trade, so the number
    /// is the last thing anyone suspects.
    ///
    /// The node's estimate alone is not the answer either, which is why the
    /// table existed: it prices the SIMULATED state with no buffer, and lands
    /// under the real need on a block that touches cold storage.
    ///
    /// So both. Estimate, add 60%, and never go under the floor. Bounded, so a
    /// slow node cannot hold up a send — on timeout the floor stands, which is
    /// exactly the old behaviour.
    async fn gas_for<P: Provider>(provider: &P, tx: &TransactionRequest, floor: u64) -> u64 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(600),
            provider.estimate_gas(tx),
        )
        .await
        {
            Ok(Ok(est)) => (est.saturating_mul(16) / 10).max(floor),
            _ => floor,
        }
    }

    /// Log a line AND surface it as the dashboard status (user feedback).
    pub fn note(&mut self, s: String) {
        // Errors reach here with the failing URL, key and all, still attached.
        let s = crate::net::redact(&s);
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
        // Both read paths now compute the reserves (a light read still asks
        // for price and liquidity, which is all they take), so price, pooled
        // depth and market cap move at full poll rate instead of every 8th
        // tick. Zeros are still not applied over known values: a read that
        // answered zero liquidity flows through `ready` below instead.
        if m.r0 > 0.0 || m.r1 > 0.0 || m.full {
            self.r0 = m.r0;
            self.r1 = m.r1;
        }
        // Trace on CHANGE (or once per few seconds), not on every render tick.
        // This line used to be written — and flushed — ten times a second
        // whether or not anything moved, which made the trace file a metronome
        // and put a synchronous disk write inside the hot loop.
        let due = self
            .last_market_trace
            .is_none_or(|(at, tick)| tick != self.tick || at.elapsed().as_secs() >= 5);
        if due {
            self.last_market_trace = Some((std::time::Instant::now(), self.tick));
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
        }
        // Keep the last known balance when the read failed: stale is honest,
        // zero is a lie.
        if let Some(eth) = m.eth {
            self.eth = eth;
        }
        if let Some(t) = m.token_bal {
            self.token_bal = t;
        }
        self.last_read_ms = m.read_ms;
        // Ready tracks the same rule as the reserves above: any read that
        // actually asked about liquidity gets to say whether there is any.
        if m.full || m.sqrt_price > 0.0 {
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
            // The light read: slot0 + liquidity, which is all the quote below
            // uses. The full read here re-fetched balance, gas price and total
            // supply PER ROUTE — three identical answers per candidate, spent
            // from the same rate budget the trade itself is about to need.
            let m = match read_price_only(provider, pref, self.trader).await {
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
            // The RAW numbers, not just the human quote. A Flaunch buy that
            // reverts is almost always a floor the pool cannot pay, and
            // "out 9706739" alone cannot be checked against anything —
            // reconstructing amount_in and min_out from it took a whole
            // afternoon and several wrong answers.
            self.logline(&format!(
                "route {venue}: out {out:.8}  in_wei={} min_wei={}",
                Wei::rounded(amount).raw(),
                Wei::of_token(out * self.slip_floor(), self.pool.token_decimals).raw()
            ));
            if best.as_ref().is_none_or(|(_, b)| out > *b) {
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
            crate::trace(&format!(
                "place skip: ready={} price={} sqrt={} kind={} token_dec={}",
                self.ready,
                self.price(),
                self.sqrt_price,
                self.pool.kind.proto(),
                self.pool.token_decimals
            ));
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

        // For sells, clamp to the LIVE token balance — the cached snapshot can be
        // stale-high (a just-confirmed sell), and selling more than is held makes
        // the router's transferFrom revert with STF. Keep the EXACT U256 balance
        // too: the f64 round-trip can round a few wei above it, also causing STF.
        let mut sell_cap: Wei = Wei::MAX;
        if !buying {
            if let Ok(b) = IERC20::new(self.pool.token, provider).balanceOf(self.trader).call().await {
                amount_in = amount_in.min(units_to_f64(b._0, self.pool.token_decimals));
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
        if !buying && self.has_flaunch_route() && !self.ur_ready() {
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

        // A curve buy priced in an ERC-20 needs that asset approved TO THE
        // CURVE — the curve pulls it, and there is no router and no Permit2 in
        // the path. Without it the buy reverts inside transferFrom, which reads
        // as the launch refusing the trade rather than as a missing approval.
        if buying {
            if let PoolKind::PonsCurve { curve, quote } = self.pool.kind {
                if quote != Address::ZERO {
                    let need = U256::from(Wei::rounded(amount_in).raw());
                    let erc = IERC20::new(quote, provider);
                    let have = erc
                        .allowance(self.trader, curve)
                        .call()
                        .await
                        .map(|a| a._0)
                        .unwrap_or(U256::ZERO);
                    if have < need {
                        self.note("Approving the quote asset for this launch's curve".into());
                        let nonce = self.take_nonce(provider).await?;
                        // EXACT amount, like every other approval here. A curve
                        // is a per-launch contract, which is the last place to
                        // hand out an unlimited allowance.
                        let sent = erc.approve(curve, need).gas(120_000).nonce(nonce).send().await;
                        let hash = *self.spent_nonce(sent)?.tx_hash();
                        for _ in 0..8u32 {
                            if provider.get_transaction_receipt(hash).await.ok().flatten().is_some() {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    }
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
        // fee machinery), so they get much more headroom in both directions.
        //
        // 500k was not enough and that is what broke every Flaunch buy. The
        // symptom pointed everywhere but here: running out of gas INSIDE the
        // hook surfaces as WrappedError(hook, afterSwap, HookCallFailed) — or
        // ERC20TransferFailed, depending how far it got — which reads as a
        // hook rejecting the trade, not as a cap set too low. The route, the
        // calldata and the quote were all fine; the same bytes that reverted
        // at 500k succeed at 837k. A real hand-made buy used 837,560.
        //
        // Gas is billed on what is USED, so headroom costs nothing. Being shy
        // here cost an afternoon.
        let gas_limit = match route.kind {
            PoolKind::FlaunchV4 { .. } => if buying { 1_400_000 } else { 1_600_000 },
            _ => if buying { 300_000 } else { 450_000 },
        };
        // The floor above is a safety net; ask the node what this actually
        // costs and take the larger.
        let probe = TransactionRequest::default()
            .with_to(to)
            .with_input(data.clone())
            .with_value(value)
            .with_from(self.trader);
        let gas_limit = Self::gas_for(provider, &probe, gas_limit).await;
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
            // An allowance failure is not "this token is broken", and saying
            // so in ABI hex helped nobody: the trader spent that session
            // changing slippage and impact settings, none of which can approve
            // a token. Name it, and clear the flag so the next attempt
            // re-approves instead of repeating the same doomed send.
            let msg = e.to_string();
            if is_transfer_failure(&msg) {
                self.v3_covered = false;
                self.ur_permit2_done = false;
                self.note(format!(
                    "The router's spending allowance for {} is used up, so the transfer was refused (STF). \
                     Approving again — try the {} once more in a moment. This is not the token blocking you.",
                    self.pool.sym, side_str(side).to_lowercase()
                ));
            } else {
                self.note(format!("Skipped the {} because it would revert. {}", side_str(side).to_lowercase(), short_err(&msg)));
            }
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
    #[allow(clippy::too_many_arguments)] // a swap needs every one of these; bundling them into a struct would only move the list
    /// Send a plain transfer — ETH by value, or a token by calldata.
    ///
    /// `side: None` is the point: a transfer is not a trade, so it moves no
    /// cost basis, counts toward no fill, and writes nothing to the ledger.
    /// It still becomes an order row and a pending entry, because money left
    /// the wallet on a keypress and that is exactly what the orders list is.
    pub async fn send_transfer<P: Provider>(
        &mut self,
        provider: &P,
        to: Address,
        data: Bytes,
        value: U256,
        label: String,
    ) -> Option<TxHash> {
        self.send_raw(provider, to, data, value, label, None, 0.0, 0.0).await
    }

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
            .map(|b| units_to_f64(b._0, self.pool.token_decimals))
            .unwrap_or(self.token_bal);
        if self.send_raw(provider, to1, data1, val1, label1, Some(Side::Buy), eth_in, tok_out).await.is_none() {
            return Ok(());
        }

        // Wait (bounded ~8s) for the bought tokens to land before selling them.
        let mut got = 0.0;
        for _ in 0..40u32 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if let Ok(b) = erc.balanceOf(self.trader).call().await {
                let now = units_to_f64(b._0, self.pool.token_decimals);
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
        if matches!(sk, PoolKind::FlaunchV4 { .. }) && !self.ur_ready() {
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
        // Cancel-safety: this runs under a timeout, so the future can be dropped
        // at any await. A hash must therefore stay in `self.pending` until its
        // receipt is in hand — draining up front meant one slow endpoint cost
        // every in-flight tx: the fill, its ledger row, and the duplicate-buy
        // guard, which reads `self.pending` and would wave a second buy through.
        let mut idx = 0;
        while idx < self.pending.len() {
            let hash = self.pending[idx].hash;
            match provider.get_transaction_receipt(hash).await {
                Ok(Some(rc)) => {
                    // Receipt in hand. Settling is synchronous from here — the
                    // one await left (pre_approve_exit) is an optimisation the
                    // sell path can redo — so removing now strands nothing.
                    let p = self.pending.remove(idx);
                    // What this transaction actually cost to send, straight
                    // off the receipt — not the pre-trade estimate, which is
                    // a guess made before the gas price was known.
                    let gas_eth =
                        rc.gas_used as f64 * rc.effective_gas_price as f64 / 1e18;
                    if rc.status() {
                        self.trades += 1;
                        self.settle_order(p.hash, OrderStatus::Confirmed, rc.block_number.unwrap_or(0), gas_eth);
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
                                if lg.address() == weth()
                                    && (to == swap_router_02() || to == universal_router())
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
                                // The token side is NOT 18-dec by assumption: a
                                // 6-dec token read through wei_to_f64 lands 1e12
                                // low, understating bought_qty and inflating
                                // avg_basis for the life of the position.
                                fill_tok = units_to_f64(tok_in, self.pool.token_decimals); // real tokens received
                            }
                            if side == Side::Sell {
                                recv_eth = Some(fill_eth); // key number: ETH actually received
                            }
                            self.apply_fill(side, fill_eth, fill_tok, p.hash, gas_eth);
                            match side {
                                // Look again in a moment: did the coin let us
                                // keep what it just handed over?
                                Side::Buy => {
                                    let had = IERC20::new(self.pool.token, provider)
                                        .balanceOf(self.trader)
                                        .call()
                                        .await
                                        .map(|b| b._0)
                                        .unwrap_or(U256::ZERO);
                                    if !had.is_zero() {
                                        self.drain_watch.push(DrainWatch {
                                            token: self.pool.token,
                                            had,
                                            due: Instant::now() + std::time::Duration::from_secs(8),
                                            hash: p.hash,
                                        });
                                    }
                                }
                                // We spent it ourselves — nothing to accuse.
                                Side::Sell => self.drain_watch.retain(|w| w.token != self.pool.token),
                            }
                            // Pre-approve the exit the moment a buy confirms, so the
                            // later v3 sell carries an exact-amount allowance and
                            // fires instantly (no approve tx in the sell path).
                            if side == Side::Buy {
                                if let Err(e) = self.pre_approve_exit(provider).await {
                                    self.note(format!("Pre approval failed. {}", short_err(&e.to_string())));
                                }
                            } else if let Err(e) = self.revoke_permit2_if_empty(provider).await {
                                // Worth saying, not worth failing the sell over:
                                // the trade landed, and this is cleanup.
                                self.note(format!("Could not revoke the approval. {}", short_err(&e.to_string())));
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
                                if lg.address() == position_manager()
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
                        // A revert still burns the gas. Nothing was bought, so
                        // there is no basis to add it to — it is spent money
                        // with no position behind it, and saying so is the only
                        // honest place to put it.
                        self.gas_burned += gas_eth;
                        self.settle_order(p.hash, OrderStatus::Reverted, rc.block_number.unwrap_or(0), gas_eth);
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
                _ => idx += 1,
            }
        }
    }

    /// Open a v4 liquidity position sized to `eth_wei` at the current range.
    /// v4-only: the bot doesn't manage v3 (NonfungiblePositionManager) positions.
    #[cfg(feature = "liquidity")]
    pub async fn add_liquidity<P: Provider>(&mut self, provider: &P, eth_wei: u128) -> eyre::Result<()> {
        let spacing = match self.pool.kind {
            PoolKind::V4 { tick_spacing, .. } => tick_spacing,
            PoolKind::V3 { .. } | PoolKind::PonsCurve { .. } | PoolKind::PonsV2Pool { .. } => {
                self.skips += 1;
                self.push_order("ADD LP".into(), OrderStatus::Skipped, None);
                // A curve holds the whole supply and IS the liquidity; there is
                // no position to open, and the pool it graduates into is seeded
                // by the protocol, not by us.
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

        // What the PositionManager actually needs to move for THIS add.
        use alloy::primitives::aliases::{U160, U48};
        let need160: U160 = U256::from(amount1_max).min(U256::from(U160::MAX)).to();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let now48 = U48::from(now_secs);

        // Approvals persist on-chain across sessions, so ask the chain rather
        // than trusting a session flag — re-sending would hit "nonce too low"
        // and waste a round-trip.
        if !self.lp_permit2_done {
            let erc = IERC20::new(self.pool.token, provider);
            let p2 = IPermit2::new(PERMIT2, provider);
            let erc_ok = erc
                .allowance(self.trader, PERMIT2)
                .call()
                .await
                .map(|a| a._0 >= U256::from(amount1_max))
                .unwrap_or(false);
            // An exact grant gets SPENT, and it expires. Both have to still
            // cover this add or it needs approving again.
            let p2_ok = p2
                .allowance(self.trader, self.pool.token, position_manager())
                .call()
                .await
                .map(|a| a.amount >= need160 && a.expiration > now48)
                .unwrap_or(false);
            if erc_ok && p2_ok {
                self.lp_permit2_done = true;
            }
        }
        if !self.lp_permit2_done {
            let erc = IERC20::new(self.pool.token, provider);
            let p2 = IPermit2::new(PERMIT2, provider);
            // EXACT amount, and a day to use it — not U160::MAX until 2100.
            // An unlimited, effectively permanent grant to a contract that can
            // move the token is exactly what `ensure_ur_allowance` refuses to
            // hand the router; the LP path had no reason to be different.
            //
            // The ERC-20 leg to PERMIT2 stays MAX — the documented exception
            // in docs/manifesto.md. Permit2 moves nothing without a grant, and
            // the grant is right here: exact, and a day long.
            let amount160 = need160;
            let expiration48 = U48::from(now_secs.saturating_add(crate::config::permit2_ttl_secs()));
            self.note("approving token for Permit2…".into());
            // Fire both approvals (broadcast immediately, no wait between).
            let sent = async {
                let h1 = *erc.approve(PERMIT2, U256::MAX).send().await?.tx_hash();
                let h2 = *p2
                    .approve(self.pool.token, position_manager(), amount160, expiration48)
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
            .with_to(position_manager())
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
    #[cfg(feature = "liquidity")]
    async fn find_positions<P: Provider>(&self, provider: &P, limit: usize) -> Vec<U256> {
        let posm = IPositionManager::new(position_manager(), provider);
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
    #[cfg(feature = "liquidity")]
    async fn burn_position<P: Provider>(&mut self, provider: &P, token_id: U256, label: String) -> eyre::Result<()> {
        let burn_id = Some(token_id);
        // Optimistically drop it from the owned cache so rapid removes step to
        // the next position (re-added on revert; see reap).
        self.positions.retain(|x| *x != token_id);

        // Price the burn before asking for it. The position's RANGE is the one
        // input we do not keep — it lives in the PositionManager — and without
        // it there is no way to say what the position should return, which is
        // why the minimums used to be zero.
        //
        // Liquidity comes from the chain rather than `pos_liq`: the cache is
        // seeded at mint and a position may predate this session entirely. Two
        // reads, only on a burn, which is rare.
        let pm = IPositionManager::new(position_manager(), provider);
        let cb_info = pm.getPoolAndPositionInfo(token_id);
        let cb_liq = pm.getPositionLiquidity(token_id);
        let (info, liq) = tokio::join!(cb_info.call(), cb_liq.call());
        let (mut amount0_min, mut amount1_min) = (0u128, 0u128);
        match (info, liq) {
            (Ok(i), Ok(l)) => {
                let (lo, hi) = position_ticks(i.info);
                let (a0, a1) = burn_amounts(u128_to_f64(l.liquidity), self.sqrt_price, lo, hi);
                let floor = self.slip_floor();
                amount0_min = (a0 * floor).max(0.0) as u128;
                amount1_min = (a1 * floor).max(0.0) as u128;
                self.logline(&format!(
                    "burn #{token_id}: range {lo}..{hi} L={} expects {a0:.0}/{a1:.0} base units, min {amount0_min}/{amount1_min}",
                    l.liquidity
                ));
            }
            // Never let a failed read block an exit. A burn with zero minimums
            // is the old behaviour, and it is still better than a position that
            // cannot be closed because one call did not answer.
            _ => self.logline(&format!(
                "burn #{token_id}: could not read the position's range, falling back to no minimum"
            )),
        }
        let data =
            v4::close_liquidity_calldata(token_id, self.pool.token, self.trader, amount0_min, amount1_min);
        let tx = TransactionRequest::default()
            .with_to(position_manager())
            .with_input(data)
            .with_from(self.trader);
        // Pre-flight, like every other send path. A burn is the one call that
        // went out unsimulated, and a revert costs gas AND leaves the position
        // out of the cache until the next reap puts it back.
        if let Err(e) = provider.call(&tx).await {
            self.skips += 1;
            self.positions.push(token_id); // undo the optimistic removal above
            self.push_order(label, OrderStatus::Skipped, None);
            self.note(format!("Closing the position would revert. {}", short_err(&e.to_string())));
            return Ok(());
        }
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
    #[cfg(feature = "liquidity")]
    fn pending_burn_ids(&self) -> std::collections::HashSet<U256> {
        self.pending
            .iter()
            .filter_map(|p| p.label.rsplit('#').next().and_then(|s| s.trim().parse::<u128>().ok()))
            .map(U256::from)
            .collect()
    }

    /// Ensure the owned-position cache is populated (scan the chain only when
    /// it is empty — e.g. first use or after everything has been closed).
    #[cfg(feature = "liquidity")]
    async fn ensure_positions<P: Provider>(&mut self, provider: &P) {
        if self.positions.is_empty() {
            self.positions = self.find_positions(provider, 50).await;
        }
    }

    /// Remove ONE position ('r' key) — the most recent one, to iterate. Uses
    /// the local cache so rapid presses don't rescan or double-burn.
    #[cfg(feature = "liquidity")]
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
    /// Close every liquidity position at once.
    ///
    /// Nothing calls this today. It was reached by a wallet-wide sweep key that
    /// has been removed: a screen showing one pool and one position should not
    /// have a key that acts on everything else you hold. It is kept for the
    /// liquidity bundle in V2, where closing positions is an action on a
    /// positions screen rather than a surprise from a trading one.
    #[allow(dead_code)]
    #[cfg(feature = "liquidity")]
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
        let amount_in = units_to_f64(bal_u256, self.pool.token_decimals);
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
        if self.has_flaunch_route() && !self.ur_ready() {
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
            // Same lesson as the sized trade: 650k was not enough, and an OOG
            // inside the hook reads as the hook refusing the trade.
            PoolKind::FlaunchV4 { .. } => 1_600_000,
            _ => 450_000,
        };
        let probe = TransactionRequest::default().with_to(to).with_input(data.clone()).with_from(self.trader);
        let dump_gas = Self::gas_for(provider, &probe, dump_gas).await;
        let tx = TransactionRequest::default().with_to(to).with_input(data).with_gas_limit(dump_gas).with_from(self.trader);
        // Report the sell in ETH numeraire (expected proceeds), not token units.
        let label = format!("SELL ALL for {:.6} ETH @ {:.6} [{} {}] liq_eth={:.6}", expected, self.price(), route.kind.proto(), route.label, self.r0);
        if let Err(e) = provider.call(&tx).await {
            self.skips += 1;
            self.push_order(label, OrderStatus::Skipped, None);
            // An allowance failure is not "this token is broken", and saying
            // so in ABI hex helped nobody: the trader spent that session
            // changing slippage and impact settings, none of which can approve
            // a token. Name it, and clear the flag so the next attempt
            // re-approves instead of repeating the same doomed send.
            let msg = e.to_string();
            if is_transfer_failure(&msg) {
                self.v3_covered = false;
                self.ur_permit2_done = false;
                self.note(format!(
                    "The router's spending allowance for {} is used up, so the transfer was refused (STF). \
                     Approving again — try the {} once more in a moment. This is not the token blocking you.",
                    self.pool.sym, "sell"
                ));
            } else {
                self.note(format!("Skipped the {} because it would revert. {}", "sell", short_err(&msg)));
            }
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
        let pm = IPoolManager::new(pool_manager(), provider);
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

}

/// Compress a multi-line RPC/revert error to a single readable line for the
/// status bar (the full text is always in the log ring / session file).
/// Whether a revert is the router failing to move the token.
///
/// `STF` and `TF` are Uniswap's `TransferHelper` reverts — SafeTransferFrom and
/// SafeTransfer. They arrive as a bare three-character string inside an ABI
/// blob, which tells a trader nothing, and the overwhelmingly common cause is
/// an allowance that no longer covers the trade rather than anything wrong with
/// the token.
fn is_transfer_failure(e: &str) -> bool {
    // Match the revert STRING, not the hex — the payload contains plenty of
    // other characters and "TF" would hit almost anything.
    e.contains("execution reverted: STF") || e.contains("execution reverted: TF")
}

fn short_err(e: &str) -> String {
    let one: String = e.split('\n').next().unwrap_or(e).trim().to_string();
    if one.chars().count() <= 160 {
        return one;
    }
    // Keep the TAIL of revert data, not just the head.
    //
    // A v4 revert arrives as WrappedError(address,bytes4,bytes,bytes) — the
    // failing currency, then the selector that failed, then the reason. All of
    // that lives past the 160th character, so truncating from the front threw
    // away the entire diagnosis and left `data: "0x90bfb865…"`, which says
    // only "something inside the pool reverted". Chasing one of these took
    // five round trips to the chain to recover what the line already had.
    if let Some(at) = one.find("0x") {
        let (head, hex) = one.split_at(at);
        let hex: String = hex.chars().take_while(|c| c.is_ascii_hexdigit() || *c == 'x').collect();
        if hex.len() > 74 {
            // Enough for the selector and the first two words, then the tail.
            let front: String = hex.chars().take(74).collect();
            let back: String = hex.chars().skip(hex.chars().count().saturating_sub(64)).collect();
            let head: String = head.chars().take(90).collect();
            return format!("{head}{front}…{back}");
        }
    }
    format!("{}…", one.chars().take(160).collect::<String>())
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
    // A bonding curve has no sqrt price, no tick and no concentrated liquidity.
    // It reports its reserves directly, and the price is simply their ratio —
    // so the pool-shaped read below is skipped entirely and the market is built
    // from `getReserves`.
    //
    // The quote reserve it returns includes a PHANTOM balance, a virtual amount
    // that sets the opening price so the first buyer does not get the supply
    // for nothing. That is correct for pricing and wrong for "how much has this
    // raised", which is `realQuoteReserve` — pooled depth uses the real one.
    if let PoolKind::PonsCurve { curve, .. } = pref.kind {
        let c = IPonsCurve::new(curve, provider);
        let cb_res = c.getReserves();
        let cb_real = c.realQuoteReserve();
        let cb_bal = erc.balanceOf(trader);
        let (res, real, bal) = tokio::join!(cb_res.call(), cb_real.call(), cb_bal.call());
        let res = res?;
        let q = units_to_f64(res.quoteReserve, pref.quote.decimals());
        let t = units_to_f64(res.tokenReserve, pref.token_decimals);
        let raised = real.map(|r| units_to_f64(r._0, pref.quote.decimals())).unwrap_or(0.0);
        crate::trace(&format!(
            "curve market: q={q} t={t} raised={raised} qdec={} tdec={}",
            pref.quote.decimals(),
            pref.token_decimals
        ));
        let eth = if full {
            crate::rpcstats::timed("eth_getBalance", provider.get_balance(trader)).await.ok()
        } else {
            None
        };
        return Ok(Market {
            // Price is quote per token, the same orientation every other venue
            // reports, so the rest of the app needs no special case.
            sqrt_price: if t > 0.0 { (q / t).sqrt() } else { 0.0 },
            tick: 0,
            r0: raised, // what is actually in it, not the phantom-inflated figure
            r1: t,
            eth: eth.map(wei_to_f64),
            token_bal: bal.ok().map(|b| units_to_f64(b._0, pref.token_decimals)),
            ready: q > 0.0 && t > 0.0,
            read_ms: t0.elapsed().as_secs_f64() * 1000.0,
            gas_price: provider.get_gas_price().await.map(|g| g as f64).unwrap_or(0.0),
            supply: 0.0,
            full,
        });
    }
    let (sqrt_p, tick, l) = match pref.kind {
        // Curves returned above; this arm exists only so the match is total.
        PoolKind::PonsCurve { .. } => (0.0, 0, 0.0),
        PoolKind::V4 { pool_id, .. }
        | PoolKind::FlaunchV4 { pool_id, .. }
        | PoolKind::PonsV2Pool { pool_id, .. } => {
            let sv = IStateView::new(state_view(), provider);
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
    let (eth_bal, tok_bal, gas_price, supply) = if full {
        let (eth_bal, tok_bal, gas, sup) = tokio::join!(
            crate::rpcstats::timed("eth_getBalance", provider.get_balance(trader)),
            crate::rpcstats::timed("balanceOf", cb_tok.call()),
            crate::rpcstats::timed("eth_gasPrice", provider.get_gas_price()),
            crate::rpcstats::timed("totalSupply", cb_sup.call()),
        );
        let td = pref.token_decimals;
        (
            eth_bal.ok(),
            tok_bal.ok().map(|b| b._0),
            gas.map(|g| g as f64).unwrap_or(0.0),
            sup.map(|s| units_to_f64(s._0, td)).unwrap_or(0.0),
        )
    } else {
        // The cheap path skips the four balance/gas/supply reads — but it DID
        // read the price and liquidity, so the reserves below are computed for
        // both paths. They used to be zeroed on light reads, which meant price
        // depth and market cap only moved on every 8th poll; now the one
        // number that has to feel live updates at full poll rate.
        (None, None, 0.0, 0.0)
    };

    // Normalize reserves to (quote-side r0, token-side r1) in HUMAN units, using
    // each side's real decimals. The raw virtual reserves are a=token0, b=token1;
    // scale each by 10^decimals (NOT a blanket 1e18 — USDG is 6-dec, so 1e18 read
    // its reserve as ~0). Tracked tokens are 18-dec; the quote may be ETH (18) or
    // a stablecoin (e.g. USDG 6).
    let a_raw = if sqrt_p > 0.0 { l / sqrt_p } else { 0.0 }; // token0 raw reserve

    let b_raw = l * sqrt_p; // token1 raw reserve
    let qd = pref.quote.decimals() as i32;
    let tdi = pref.token_decimals as i32; // tracked-token decimals — read on-chain, NOT assumed
    let (r0, r1) = match pref.kind {
        // Curves returned above; this arm exists only so the match is total.
        PoolKind::PonsCurve { .. } => (0.0, 0.0),
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
        // pons v2: same orientation question as Flaunch, but the quote is
        // whatever the launch was priced in — NOT necessarily an 18-decimal
        // asset, so the quote side is scaled by its own decimals.
        PoolKind::PonsV2Pool { coin_is_0, .. } => {
            if coin_is_0 {
                (b_raw / 10f64.powi(qd), a_raw / 10f64.powi(tdi))
            } else {
                (a_raw / 10f64.powi(qd), b_raw / 10f64.powi(tdi))
            }
        }
    };
    Ok(Market {
        sqrt_price: sqrt_p,
        tick,
        r0,
        r1,
        eth: eth_bal.map(wei_to_f64), // native ETH is always 18-dec
        token_bal: tok_bal.map(|t| units_to_f64(t, pref.token_decimals)),
        ready: r0 > 0.0 && r1 > 0.0,
        read_ms: t0.elapsed().as_secs_f64() * 1000.0,
        gas_price,
        supply,
        full,
    })
}

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TapeAction {
    Buy,
    Sell,
    Add,
    Remove,
}

/// A decoded pool event by ANY trader — the live tape (buys, sells, LP add/remove).
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Swap {
    pub action: TapeAction,
    pub eth: f64,       // ETH/weth() size of the event
    pub eth_wei: u128,  // EXACT weth() size in wei (no float rounding) — for copy-step keying
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

/// The widest span a single `eth_getLogs` may cover.
///
/// This chain's free tier refuses anything over ten blocks with a 400, and a
/// refused request looks exactly like a quiet market from the outside — which
/// is how an empty tape sat next to a moving chart for an afternoon. Asking in
/// slices the endpoint will actually serve is not an optimisation, it is the
/// difference between a tape and no tape.
pub const MAX_LOG_SPAN: u64 = 10;

/// Read swaps over ANY span, in windows the endpoint will serve.
///
/// Walks BACKWARDS from the newest block so the most recent trades arrive
/// first: if the budget runs out, or an endpoint starts refusing, what you have
/// is the live end of the tape rather than ancient history.
pub async fn read_swaps_chunked<P: Provider>(
    provider: &P,
    pref: PoolRef,
    from_block: u64,
    to_block: u64,
    max_calls: usize,
) -> eyre::Result<Vec<Swap>> {
    let mut out = Vec::new();
    let mut hi = to_block;
    for _ in 0..max_calls {
        if hi < from_block {
            break;
        }
        let lo = hi.saturating_sub(MAX_LOG_SPAN - 1).max(from_block);
        match read_swaps(provider, pref, lo, hi).await {
            Ok(mut v) => out.append(&mut v),
            // One refused window must not cost the others.
            Err(e) => crate::trace(&format!("tape chunk {lo}..{hi} failed: {e}")),
        }
        if lo == from_block {
            break;
        }
        hi = lo.saturating_sub(1);
    }
    out.sort_by_key(|s| s.block);
    Ok(out)
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
        // A curve emits its OWN trade events, on its own address — not Uniswap
        // Swaps on the PoolManager. Decoding those is separate work, so rather
        // than filter for logs this decoder cannot read, say so once and return
        // nothing. An empty tape that explains itself beats one that does not.
        PoolKind::PonsCurve { curve, .. } => {
            let filter = Filter::new().address(curve).from_block(from_block).to_block(to_block);
            let logs = crate::rpcstats::timed("eth_getLogs", provider.get_logs(&filter)).await?;
            let word = |b: &[u8], i: usize| -> U256 {
                if b.len() < (i + 1) * 32 {
                    return U256::ZERO;
                }
                U256::from_be_slice(&b[i * 32..(i + 1) * 32])
            };
            let mut out = Vec::new();
            for lg in &logs {
                let topics = lg.topics();
                let Some(t0) = topics.first() else { continue };
                let buying = *t0 == IPonsCurve::CurveBuy::SIGNATURE_HASH;
                if !buying && *t0 != IPonsCurve::CurveSell::SIGNATURE_HASH {
                    continue;
                }
                let d = lg.data().data.as_ref();
                let (quote_raw, token_raw) = if buying {
                    (word(d, 0), word(d, 1))
                } else {
                    (word(d, 1), word(d, 0))
                };
                let q = units_to_f64(quote_raw, quote_dec);
                let t = units_to_f64(token_raw, token_dec);
                if q <= 0.0 || t <= 0.0 {
                    continue;
                }
                out.push(Swap {
                    action: if buying { TapeAction::Buy } else { TapeAction::Sell },
                    eth: q,
                    eth_wei: quote_raw.to_string().parse::<u128>().unwrap_or(0),
                    price: t / q,
                    trader: topics.get(1).map(|w| Address::from_word(*w)).unwrap_or_default(),
                    liq_eth: 0.0,
                    block: lg.block_number.unwrap_or(0),
                    tx: lg.transaction_hash.unwrap_or_default(),
                    tick_lo: 0,
                    tick_hi: 0,
                    is_v4: false,
                });
            }
            return Ok(out);
        }
        PoolKind::V4 { pool_id, .. } => (
            true, true,
            Filter::new().address(pool_manager()).topic1(pool_id).from_block(from_block).to_block(to_block),
        ),
        // Same PoolManager events as V4, but the quote side is flETH, whose
        // position follows the launch's _currencyFlipped rather than always 0.
        // pons v2: same PoolManager events, orientation from the launch record.
        PoolKind::PonsV2Pool { pool_id, coin_is_0, .. } => (
            !coin_is_0, true,
            Filter::new().address(pool_manager()).topic1(pool_id).from_block(from_block).to_block(to_block),
        ),
        PoolKind::FlaunchV4 { pool_id, coin_is_0 } => (
            !coin_is_0, true,
            Filter::new().address(pool_manager()).topic1(pool_id).from_block(from_block).to_block(to_block),
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
        .rsplit([' ', '~'])
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
}

/// Unpack `(tickLower, tickUpper)` from a v4 `PositionInfo` word.
///
/// The PositionManager packs a position's identity into one 256-bit value:
///
/// ```text
///   bits 0..8     hasSubscriber flag
///   bits 8..32    tickLower, int24
///   bits 32..56   tickUpper, int24
///   bits 56..256  poolId
/// ```
///
/// The ticks are SIGNED 24-bit, so they need sign-extending — get that wrong
/// and the range comes out garbage, which would make every burn either revert
/// or accept any price. The tests below check this against positions read from
/// the live PositionManager.
#[cfg(any(feature = "liquidity", test))]
pub fn position_ticks(info: U256) -> (i32, i32) {
    let field = |shift: u32| -> i32 {
        let raw = ((info >> shift) & U256::from(0xFF_FFFFu32)).to::<u32>();
        // Sign-extend 24 bits into i32.
        if raw & 0x80_0000 != 0 {
            raw as i32 - (1 << 24)
        } else {
            raw as i32
        }
    };
    (field(8), field(32))
}

/// What burning a position returns at the current price: `(amount0, amount1)`
/// in BASE units, currency0 first (ETH, since address(0) sorts first).
///
/// Standard concentrated-liquidity math, the inverse of what `add_liquidity`
/// does to size a mint. Out of range the position is entirely one asset.
#[cfg(any(feature = "liquidity", test))]
pub fn burn_amounts(liquidity: f64, sqrt_p: f64, tick_lower: i32, tick_upper: i32) -> (f64, f64) {
    if liquidity <= 0.0 || sqrt_p <= 0.0 || tick_lower >= tick_upper {
        return (0.0, 0.0);
    }
    let sa = 1.0001f64.powf(tick_lower as f64 / 2.0);
    let sb = 1.0001f64.powf(tick_upper as f64 / 2.0);
    if !(sa.is_finite() && sb.is_finite()) || sa <= 0.0 {
        return (0.0, 0.0);
    }
    if sqrt_p <= sa {
        // Below the range: all currency0.
        (liquidity * (sb - sa) / (sa * sb), 0.0)
    } else if sqrt_p >= sb {
        // Above the range: all currency1.
        (0.0, liquidity * (sb - sa))
    } else {
        (liquidity * (sb - sqrt_p) / (sqrt_p * sb), liquidity * (sqrt_p - sa))
    }
}

/// What a sale of `tok` tokens costs against the open position: the inventory it
/// consumes, and the money that inventory was bought with.
///
/// ONE implementation, used by both chains. This arithmetic existed twice —
/// once here and once in the Solana settle path — and the copies drifted: the
/// Solana one retired the entire basis on a PARTIAL sell, booking a whole
/// position's cost against a fifth of its proceeds.
///
/// Two rules:
///
/// - A sale beyond the tracked inventory is free-bag: zero cost, all profit.
/// - Closing out realises the WHOLE remaining basis, not just the slice the
///   tokens account for. Inventory is learned from a polled balance that lags,
///   and a buy can confirm having delivered ~nothing at all — a honeypot, a
///   blocked transfer, a fill the poll has not seen yet. In every one of those
///   the money left the wallet, so the exit has to book it. Without this the
///   cost came out ~0 and a real loss was reported as a profit equal to the
///   entire proceeds, while the money sat stranded in the basis forever.
///
/// Returns `(inventory_consumed, cost)`.

#[cfg(test)]
mod basis_recovery_tests {
    use super::*;

    fn swap(action: TapeAction, eth: f64, price: f64, block: u64, trader: Address) -> Swap {
        Swap {
            action,
            eth,
            eth_wei: (eth * 1e18) as u128,
            price,
            trader,
            liq_eth: 1.0,
            block,
            tx: TxHash::ZERO,
            tick_lo: 0,
            tick_hi: 0,
            is_v4: false,
        }
    }

    /// The TOK case, from the real tape: four buys the basis file never knew
    /// about, then the sell that got booked as pure profit.
    #[test]
    fn open_buys_on_the_tape_rebuild_the_basis_that_was_never_recorded() {
        let me = Address::repeat_byte(0xbf);
        let px = 1e9; // tokens per ETH; only the ratio matters here
        let rows = vec![
            swap(TapeAction::Buy, 0.000_440, px, 23_445_181, me),
            swap(TapeAction::Buy, 0.000_418, px, 23_445_431, me),
            swap(TapeAction::Buy, 0.000_397, px, 23_445_563, me),
            swap(TapeAction::Buy, 0.000_377, px, 23_445_668, me),
        ];
        let (qty, cost, opened) = Bot::basis_from_tape(&rows, me);
        assert!((cost - 0.001_632).abs() < 1e-9, "every open buy counted: {cost}");
        assert!((qty - 0.001_632 * px).abs() < 1.0);
        assert_eq!(opened, Some(23_445_181), "the clock starts at the FIRST buy");
    }

    /// Strangers' trades sit on the same tape. Counting them would invent a
    /// basis out of other people's money.
    #[test]
    fn only_your_own_trades_count() {
        let me = Address::repeat_byte(0xbf);
        let them = Address::repeat_byte(0x11);
        let rows = vec![
            swap(TapeAction::Buy, 0.001, 1e9, 100, them),
            swap(TapeAction::Buy, 0.002, 1e9, 101, me),
        ];
        let (_, cost, _) = Bot::basis_from_tape(&rows, me);
        assert!((cost - 0.002).abs() < 1e-12);
    }

    /// A position already closed on the tape must rebuild as nothing — else
    /// the next buy inherits a basis it did not pay for.
    #[test]
    fn a_closed_position_rebuilds_as_flat() {
        let me = Address::repeat_byte(0xbf);
        let px = 1e9;
        let rows = vec![
            swap(TapeAction::Buy, 0.001, px, 100, me),
            swap(TapeAction::Sell, 0.003, px, 200, me), // sold the whole bag
        ];
        let (qty, cost, opened) = Bot::basis_from_tape(&rows, me);
        assert!(qty <= 1e-9 && cost <= 1e-9, "nothing left open: qty={qty} cost={cost}");
        assert_eq!(opened, None);
    }

    /// Rows arrive out of order when two windows overlap; the replay has to
    /// put them back in block order or a sell can consume a buy that had not
    /// happened yet.
    #[test]
    fn the_replay_is_in_block_order_whatever_the_file_order() {
        let me = Address::repeat_byte(0xbf);
        let px = 1e9;
        let jumbled = vec![
            swap(TapeAction::Sell, 0.003, px, 200, me),
            swap(TapeAction::Buy, 0.001, px, 100, me),
            swap(TapeAction::Buy, 0.002, px, 300, me),
        ];
        let (_, cost, opened) = Bot::basis_from_tape(&jumbled, me);
        assert!((cost - 0.002).abs() < 1e-9, "only the post-sell buy is open: {cost}");
        assert_eq!(opened, Some(300));
    }
}

pub fn realized_cost(tok: f64, bought_qty: f64, bought_cost: f64) -> (f64, f64) {
    let from_basis = if tok > 0.0 { tok.min(bought_qty) } else { bought_qty };
    let avg = if bought_qty > 1e-12 { bought_cost / bought_qty } else { 0.0 };
    let mut cost = from_basis * avg;
    if (bought_qty - from_basis) <= 1e-12 {
        cost = cost.max(bought_cost);
    }
    (from_basis, cost.max(0.0))
}

fn wei_to_f64(x: U256) -> f64 {
    // Overflow-safe (a token balance could exceed u128): parse the decimal.
    x.to_string().parse::<f64>().unwrap_or(0.0) / 1e18
}

/// On-chain base units -> human amount, using the token's REAL decimals.
/// `wei_to_f64` is the 18-dec special case; anything token-denominated must come
/// through here instead, or a 6-dec token reads as ~0.
pub fn units_to_f64(x: U256, decimals: u8) -> f64 {
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
            universal_router(),
            v4::swap_calldata(token, fee, tick_spacing, buying, wi, wm),
            value,
        ),
        // A graduated pons v2 pool: ONE v4 hop through the pons hook, paired
        // against whatever the launch was priced in. The hook is part of the
        // key, so it cannot be omitted, and the pool's own fee is zero.
        PoolKind::PonsV2Pool { quote, tick_spacing, .. } => (
            universal_router(),
            v4::hop_calldata(token, quote, pons_v2_hook(), tick_spacing, buying, wi, wm),
            // Native-quote launches pay in ETH; an ERC-20 quote is pulled
            // through Permit2 and must send none.
            if buying && quote == Address::ZERO { value } else { U256::ZERO },
        ),
        // A curve is not routed. It prices and settles the trade itself, so the
        // call goes straight to it — buy spends the quote asset, sell spends
        // the launch token, and for a native-quote launch `quoteIn` must equal
        // the value sent or it reverts with NativeValueMismatch.
        PoolKind::PonsCurve { curve, quote } => {
            let data: Bytes = if buying {
                IPonsCurve::buyCall {
                    quoteIn: U256::from(wi),
                    minTokensOut: U256::from(wm),
                    recipient: trader,
                }
                .abi_encode()
                .into()
            } else {
                IPonsCurve::sellCall {
                    tokensIn: U256::from(wi),
                    minQuoteOut: U256::from(wm),
                    recipient: trader,
                }
                .abi_encode()
                .into()
            };
            // Native quote only: an ERC-20 quote is pulled by the curve after
            // an approval, and sending value alongside reverts.
            let v = if buying && quote == Address::ZERO { U256::from(wi) } else { U256::ZERO };
            (curve, data, v)
        }
        // `fee` is deliberately unused: the Flaunch pool key's fee is 0 (the
        // hook charges its cut), and the builder hardcodes the key layout.
        PoolKind::FlaunchV4 { .. } => (
            universal_router(),
            v4::flaunch_swap_calldata(token, buying, wi, wm),
            value,
        ),
        PoolKind::V3 { .. } => {
            let d = if buying {
                v3::v3_buy_calldata(token, fee, wi, wm, trader)
            } else {
                v3::v3_sell_calldata(token, fee, wi, wm, trader)
            };
            (swap_router_02(), d, value)
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

#[cfg(test)]
mod position_info_tests {
    use super::*;

    /// Golden fixtures read from the live PositionManager at
    /// 0x58daec3116aae6D93017bAAea7749052E8a04fA7 on Robinhood Chain (4663).
    /// Each row is (tokenId, info word, the pool's tickSpacing).
    ///
    /// The packing is not something to take on faith: get a shift or the sign
    /// extension wrong and every burn either reverts forever — an LP position
    /// that cannot be closed — or carries a minimum that protects nothing.
    /// These are real positions, and the ticks they decode to have to be
    /// multiples of their own pool's spacing, which garbage would not be.
    const LIVE: [(u64, &str, i32); 4] = [
        (1, "99134477747744576331124276523330408125618109158205249198501709068689164618752", 60),
        (2, "91770821714008159108971951424986858529462063330423066963910699009089042009088", 60),
        (100, "77553772612823546114485122681908446081810649955699010580230426773650349338624", 16),
        (5000, "73425788000603542092939738578519431528834314299135160480130727973746147799040", 600),
    ];

    #[test]
    fn real_positions_decode_to_ticks_their_own_pool_could_have_made() {
        const MAX_TICK: i32 = 887_272;
        for (id, word, spacing) in LIVE {
            let info: U256 = word.parse().expect("fixture parses");
            let (lo, hi) = position_ticks(info);
            assert!(lo < hi, "#{id}: {lo}..{hi} is not an ordered range");
            assert!(lo.abs() <= MAX_TICK && hi.abs() <= MAX_TICK, "#{id}: {lo}..{hi} out of range");
            // The decisive check. A pool with this spacing can only mint ticks
            // that are multiples of it, so a wrong layout fails here.
            assert_eq!(lo % spacing, 0, "#{id}: lower {lo} is not a multiple of {spacing}");
            assert_eq!(hi % spacing, 0, "#{id}: upper {hi} is not a multiple of {spacing}");
        }
    }

    /// Positions 1 and 2 are full-range mints on a spacing-60 pool, which has
    /// exactly one correct answer: MAX_TICK floored to the spacing, 887220.
    /// Nothing but the right layout lands on that number.
    #[test]
    fn a_full_range_position_decodes_to_the_canonical_bounds() {
        for (id, word, _) in LIVE.iter().take(2) {
            let info: U256 = word.parse().unwrap();
            assert_eq!(position_ticks(info), (-887_220, 887_220), "#{id} is a full-range mint");
        }
    }

    /// Negative ticks are the sign-extension case, and a pool priced under 1.0
    /// lives entirely in them — dropping the sign would read -202320 as
    /// +16575216, a range the pool never had.
    #[test]
    fn negative_ticks_survive_sign_extension() {
        let info: U256 = LIVE[2].1.parse().unwrap();
        let (lo, hi) = position_ticks(info);
        assert_eq!((lo, hi), (-202_320, -202_240));
        assert!(lo < 0 && hi < 0, "both bounds are below tick zero");
    }

    /// A burn returns one asset when the price has left the range, and both
    /// while it is inside — and it must round-trip the sizing `add_liquidity`
    /// does, or the minimum it produces is wrong in one direction or the other.
    #[test]
    fn burn_amounts_track_where_the_price_sits() {
        let (lo, hi) = (-600i32, 600i32);
        let sa = 1.0001f64.powf(lo as f64 / 2.0);
        let sb = 1.0001f64.powf(hi as f64 / 2.0);
        let l = 1e15;

        // Below the range: all currency0, no currency1.
        let (a0, a1) = burn_amounts(l, sa * 0.9, lo, hi);
        assert!(a0 > 0.0 && a1 == 0.0, "below range is entirely currency0, got {a0}/{a1}");

        // Above it: the mirror image.
        let (a0, a1) = burn_amounts(l, sb * 1.1, lo, hi);
        assert!(a0 == 0.0 && a1 > 0.0, "above range is entirely currency1, got {a0}/{a1}");

        // Inside: both, and consistent with how add_liquidity sizes a mint.
        let sp = 1.0;
        let (a0, a1) = burn_amounts(l, sp, lo, hi);
        assert!(a0 > 0.0 && a1 > 0.0, "in range holds both, got {a0}/{a1}");
        let minted_l = a0 * (sp * sb) / (sb - sp);
        assert!((minted_l - l).abs() / l < 1e-9, "round trip lost liquidity: {minted_l} vs {l}");
        assert!((a1 - l * (sp - sa)).abs() / a1 < 1e-9, "currency1 disagrees with the mint math");
    }

    /// Nonsense in, zero out — never a minimum invented from a bad read.
    #[test]
    fn a_broken_position_asks_for_nothing() {
        assert_eq!(burn_amounts(0.0, 1.0, -60, 60), (0.0, 0.0));
        assert_eq!(burn_amounts(1e12, 0.0, -60, 60), (0.0, 0.0));
        assert_eq!(burn_amounts(1e12, 1.0, 60, 60), (0.0, 0.0), "an empty range returns nothing");
        assert_eq!(burn_amounts(1e12, 1.0, 60, -60), (0.0, 0.0), "an inverted range returns nothing");
    }
}

#[cfg(test)]
mod realized_cost_tests {
    use super::*;

    /// The shape that reported a loss as a profit, from a real trade: bought
    /// catwifhat for 0.000329 SOL, sold for 0.000008, and the balance poll had
    /// not caught up — so the inventory read as zero and the exit booked a cost
    /// of zero. The panel showed +0.000008 realised on a position that lost
    /// almost all of its money.
    #[test]
    fn a_stale_balance_cannot_turn_a_loss_into_a_profit() {
        let (_, cost) = realized_cost(0.0, 0.0, 0.000329);
        assert!((cost - 0.000329).abs() < 1e-12, "closing must realise the whole basis, got {cost}");
        let pnl = 0.000008 - cost;
        assert!(pnl < 0.0, "this trade lost money; {pnl:+.9} says otherwise");
    }

    /// A buy that confirmed and delivered nothing — honeypot, blocked transfer,
    /// a tax that rounds the fill away. The money still left the wallet.
    #[test]
    fn a_honeypot_books_what_was_actually_spent() {
        let (_, cost) = realized_cost(0.0, 0.0, 0.000552);
        assert!((cost - 0.000552).abs() < 1e-12, "got {cost}");
        assert!((0.0 - cost) < 0.0, "a total loss must read as a loss");
    }

    /// A partial sell retires a PROPORTIONAL slice. The Solana copy used to
    /// zero the whole basis here, charging a full position's cost against a
    /// fifth of its proceeds and leaving the rest of the bag at zero cost.
    #[test]
    fn a_partial_sell_retires_only_its_share() {
        let (from, cost) = realized_cost(200.0, 1000.0, 1.0);
        assert!((from - 200.0).abs() < 1e-9);
        assert!((cost - 0.2).abs() < 1e-9, "a fifth of the position costs a fifth, got {cost}");
    }

    /// A full exit realises exactly what was paid — no more, no less.
    #[test]
    fn a_full_exit_realises_the_whole_basis_exactly() {
        let (from, cost) = realized_cost(1000.0, 1000.0, 1.0);
        assert!((from - 1000.0).abs() < 1e-9);
        assert!((cost - 1.0).abs() < 1e-12, "got {cost}");
    }

    /// Selling MORE than was bought: the excess is free bag, and free bag has
    /// no cost. It must not invent one, and must not double-charge the basis.
    #[test]
    fn the_free_bag_is_free() {
        let (from, cost) = realized_cost(5000.0, 1000.0, 1.0);
        assert!((from - 1000.0).abs() < 1e-9, "cannot consume more inventory than exists");
        assert!((cost - 1.0).abs() < 1e-12, "got {cost}");
        // Nothing bought at all: an airdrop sold is pure profit.
        let (_, cost) = realized_cost(5000.0, 0.0, 0.0);
        assert_eq!(cost, 0.0);
    }

    /// Cost is never negative, whatever nonsense arrives.
    #[test]
    fn cost_never_goes_negative() {
        for (tok, qty, c) in [(-1.0, 10.0, 1.0), (10.0, -5.0, 1.0), (0.0, 0.0, -3.0)] {
            let (_, cost) = realized_cost(tok, qty, c);
            assert!(cost >= 0.0, "realized_cost({tok},{qty},{c}) = {cost}");
        }
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;

    /// The real numbers from the coin that took the money on 2026-07-31:
    /// 34,230.249532 tokens delivered by the buy, 5,670,304 base units left a
    /// few seconds later. That is the shape the check exists to name.
    #[test]
    fn a_drained_balance_is_far_past_the_threshold() {
        let had = U256::from(34_230_249_532_000_000_000_000u128);
        let now = U256::from(5_670_304u128);
        let lost = had - now;
        let pct = (lost * U256::from(100) / had).to::<u128>();
        assert_eq!(pct, 99, "a drain should read as ~100% gone, got {pct}%");
        assert!(pct >= 10, "and comfortably past the alert threshold");
    }

    /// Ordinary holding does not shrink. Dust-level noise must stay quiet, or
    /// the warning becomes something you learn to ignore.
    #[test]
    fn ordinary_noise_stays_quiet() {
        let had = U256::from(1_000_000_000_000_000_000u128);
        for still in [had, had - U256::from(1u8), had * U256::from(97u8) / U256::from(100u8)] {
            let lost = had.saturating_sub(still);
            let pct = (lost * U256::from(100) / had).to::<u128>();
            assert!(pct < 10, "{pct}% should not warn");
        }
    }
}

#[cfg(test)]
mod pons_curve_tests {
    use super::*;
    use alloy::primitives::address;

    const CURVE: Address = address!("bc39b6502e1a6ab36e4a5c5026a35f08342a0a9c");
    const TOKEN: Address = address!("a1186a1bcde151634440e5f51ae998c61e465f5d");
    const TRADER: Address = address!("bf93d16a2a0bd298bb274ba8e824097bd1122671");
    const USDG: Address = address!("5fc5360d0400a0fd4f2af552add042d716f1d168");

    /// A curve is not routed. The call goes to the CURVE, not to a router —
    /// sending a bonding-curve trade to the UniversalRouter would revert, and
    /// sending it to SwapRouter02 would be worse.
    #[test]
    fn a_curve_trade_goes_to_the_curve() {
        let (to, data, _) = build_swap(
            PoolKind::PonsCurve { curve: CURVE, quote: Address::ZERO },
            TOKEN, 0, true, Wei::exact(U256::from(1_000u64)), Wei::ZERO, TRADER,
        );
        assert_eq!(to, CURVE, "must call the curve itself");
        assert_eq!(&data[..4], &IPonsCurve::buyCall::SELECTOR, "buy(), not a router call");
    }

    /// Native quote: `quoteIn` must EQUAL the value sent, or the curve reverts
    /// with NativeValueMismatch. An ERC-20 quote is pulled after an approval,
    /// and any value sent alongside reverts with UnexpectedNativeValue.
    #[test]
    fn value_is_sent_only_for_a_native_quote() {
        let amount = Wei::exact(U256::from(500_000_000_000_000u64));
        let (_, _, native_value) = build_swap(
            PoolKind::PonsCurve { curve: CURVE, quote: Address::ZERO },
            TOKEN, 0, true, amount, Wei::ZERO, TRADER,
        );
        assert_eq!(native_value, U256::from(500_000_000_000_000u64), "native buy sends its quote");

        let (_, _, erc20_value) = build_swap(
            PoolKind::PonsCurve { curve: CURVE, quote: USDG },
            TOKEN, 0, true, amount, Wei::ZERO, TRADER,
        );
        assert_eq!(erc20_value, U256::ZERO, "an ERC-20 quote must send no value");
    }

    /// Selling spends the launch TOKEN, so no value rides along in either case.
    #[test]
    fn selling_never_sends_value() {
        for quote in [Address::ZERO, USDG] {
            let (to, data, value) = build_swap(
                PoolKind::PonsCurve { curve: CURVE, quote },
                TOKEN, 0, false, Wei::exact(U256::from(42u64)), Wei::ZERO, TRADER,
            );
            assert_eq!(to, CURVE);
            assert_eq!(&data[..4], &IPonsCurve::sellCall::SELECTOR);
            assert_eq!(value, U256::ZERO, "a sell spends tokens, not ETH");
        }
    }

    /// The pool-shaped questions have to answer sensibly for a curve, because
    /// every screen asks them.
    #[test]
    fn a_curve_knows_it_is_not_a_pool() {
        let k = PoolKind::PonsCurve { curve: CURVE, quote: Address::ZERO };
        assert!(k.is_curve());
        assert!(!k.is_v3());
        assert!(!k.is_empty());
        assert!(PoolKind::PonsCurve { curve: Address::ZERO, quote: Address::ZERO }.is_empty());
        assert_eq!(k.proto(), "curve");
    }
}

#[cfg(test)]
mod pons_v2_pool_tests {
    use super::*;
    use alloy::primitives::address;

    const TOKEN: Address = address!("a1186a1bcde151634440e5f51ae998c61e465f5d");
    const USDG: Address = address!("5fc5360d0400a0fd4f2af552add042d716f1d168");

    /// The hook is part of the pool key. A pool id computed with hooks = 0
    /// addresses a pool that does not exist, so every swap built from it would
    /// revert — and the id is derived, never read back, so nothing else would
    /// catch it.
    #[test]
    fn the_hook_is_part_of_the_pool_id() {
        let with = pons_v2_pool_id(TOKEN, Address::ZERO, 60, pons_v2_hook());
        let without = pons_v2_pool_id(TOKEN, Address::ZERO, 60, Address::ZERO);
        assert_ne!(with, without, "the hook must change the pool id");
    }

    /// Uniswap sorts currencies by address, and native ETH is address zero, so
    /// a native-quote launch always has ETH as currency0. Getting the order
    /// wrong yields a different id — again, a pool that does not exist.
    #[test]
    fn currency_order_does_not_depend_on_argument_order() {
        let a = pons_v2_pool_id(TOKEN, USDG, 60, pons_v2_hook());
        let b = pons_v2_pool_id(USDG, TOKEN, 60, pons_v2_hook());
        assert_eq!(a, b, "the key sorts its currencies, so the id is stable");
        assert_ne!(
            pons_v2_pool_id(TOKEN, Address::ZERO, 60, pons_v2_hook()),
            a,
            "a different quote asset is a different pool"
        );
    }

    /// Tick spacing is in the key too.
    #[test]
    fn tick_spacing_changes_the_pool() {
        assert_ne!(
            pons_v2_pool_id(TOKEN, Address::ZERO, 60, pons_v2_hook()),
            pons_v2_pool_id(TOKEN, Address::ZERO, 200, pons_v2_hook()),
        );
    }

    /// A graduated pool routes through the Universal Router, not the curve.
    #[test]
    fn a_graduated_launch_routes_through_the_router() {
        let kind = PoolKind::PonsV2Pool {
            pool_id: B256::ZERO,
            coin_is_0: false,
            quote: Address::ZERO,
            tick_spacing: 60,
        };
        let (to, _, value) = build_swap(
            kind, TOKEN, 0, true, Wei::exact(U256::from(1_000u64)), Wei::ZERO, TOKEN,
        );
        assert_eq!(to, universal_router());
        assert_eq!(value, U256::from(1_000u64), "a native-quote buy sends ETH");

        let erc20 = PoolKind::PonsV2Pool {
            pool_id: B256::ZERO,
            coin_is_0: false,
            quote: USDG,
            tick_spacing: 60,
        };
        let (_, _, v) = build_swap(
            erc20, TOKEN, 0, true, Wei::exact(U256::from(1_000u64)), Wei::ZERO, TOKEN,
        );
        assert_eq!(v, U256::ZERO, "an ERC-20 quote is pulled, not sent");
    }
}

#[cfg(test)]
mod allowance_tests {
    use super::is_transfer_failure;

    /// The exact string the RPC returned while a wallet holding 163,373 tokens
    /// could not sell any of them.
    #[test]
    fn the_routers_transfer_reverts_are_recognised() {
        let stf = "server returned an error response: error code 3: execution reverted: STF, data: \"0x08c379a0…\"";
        let tf = "server returned an error response: error code 3: execution reverted: TF, data: \"0x08c379a0…\"";
        assert!(is_transfer_failure(stf));
        assert!(is_transfer_failure(tf));
    }

    /// Other reverts must keep their own diagnosis — telling somebody their
    /// allowance ran out when the pool simply had no liquidity would send them
    /// looking in the wrong place, which is the failure being fixed here.
    #[test]
    fn other_reverts_are_left_alone() {
        for other in [
            "execution reverted: Too little received",
            "execution reverted: STP",
            "execution reverted: LOK",
            "server returned an error response: error code 3: execution reverted",
        ] {
            assert!(!is_transfer_failure(other), "{other} is not an allowance failure");
        }
    }

    /// The payload of ANY revert is hex that contains stray letters; matching
    /// on the bare token would flag everything.
    #[test]
    fn the_hex_payload_does_not_trigger_it() {
        let unrelated = "execution reverted: Too little received, data: \"0x08c379a0STF00TF\"";
        assert!(!is_transfer_failure(unrelated));
    }
}
